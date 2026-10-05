// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";
import { api } from "../lib/api";
import {
  addressHost,
  canContinue,
  createRandomNetworkName,
  formatAdvertiseAddress,
  isReadyToSave,
  JoinScreen,
  LocalDiscoveryPanel,
  NetworkScreen,
  PublicDirectoryPanel,
  StartScreen,
  TrustFields,
  type SetupState,
} from "./Setup";

const base: SetupState = {
  profile: "infer",
  joinAccepts: null,
  publicDirectory: "",
  connectionKey: "",
  advertiseAddr: "",
  trustDocument: "",
  trustFingerprint: "",
  nodeName: "node",
  network: {
    public_name: "",
    organization: "",
    description: "",
    rules: "",
    channels: "",
    visibility: "private",
    access_policy: "anyone_with_anchor",
    country_code: "",
    country_name: "",
    city: "",
    geo_label: "",
    location: null,
  },
  ramReservePct: 20,
  cpuReservePct: 20,
  modelDirs: [],
  shardDir: "",
  shardMaxGb: 20,
  thermalLimitC: 85,
};

const t = (key: string, values?: Record<string, string | number>) =>
  values ? `${key}:${Object.values(values).join(",")}` : key;

function wrap(node: React.ReactNode) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <ToastProvider>{node}</ToastProvider>
    </LocaleProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("helpers", () => {
  it("creates a short editable network name", () => {
    expect(createRandomNetworkName(0)).toBe("POM-00000");
    expect(createRandomNetworkName(0.5)).toMatch(/^POM-[0-9A-Z]{5}$/);
  });

  it("formats the advertised address with the P2P port", () => {
    expect(formatAdvertiseAddress("2804::1", 42777)).toBe("[2804::1]:42777");
    expect(formatAdvertiseAddress("10.0.0.5", 42777)).toBe("10.0.0.5:42777");
  });

  it("shows the IP of a discovered machine without port or brackets", () => {
    expect(addressHost("192.168.1.42:42778")).toBe("192.168.1.42");
    expect(addressHost("[fe80::1]:42778")).toBe("fe80::1");
  });
});

describe("what the wizard requires", () => {
  const trusted = { trustDocument: "{}", trustFingerprint: "abc" };

  it("does not ask a private admin network for its city", () => {
    const admin: SetupState = { ...base, ...trusted, profile: "admin", network: { ...base.network, public_name: "POM-1" } };
    expect(canContinue("setup-network", admin)).toBe(true);
    expect(isReadyToSave(admin)).toBe(true);
  });

  it("never requires an admin to pin an address: the network is found by key", () => {
    const admin: SetupState = { ...base, ...trusted, profile: "admin", advertiseAddr: "", network: { ...base.network, public_name: "POM-1" } };
    expect(canContinue("setup-network", admin)).toBe(true);
    expect(isReadyToSave(admin)).toBe(true);
  });

  it("asks a public admin network for country and city, for the pkarr record", () => {
    const admin: SetupState = { ...base, ...trusted, profile: "admin", network: { ...base.network, public_name: "POM-1", visibility: "public" } };
    expect(canContinue("setup-network", admin)).toBe(false);
    const located = { ...admin, network: { ...admin.network, country_code: "BR", city: "Curitiba" } };
    expect(canContinue("setup-network", located)).toBe(true);
  });

  it("lets a node finish with the trust and the key only", () => {
    expect(isReadyToSave({ ...base, ...trusted, profile: "node" })).toBe(false);
    expect(isReadyToSave({ ...base, ...trusted, profile: "node", connectionKey: "secret" })).toBe(true);
    expect(isReadyToSave({ ...base, ...trusted, profile: "node", network: { ...base.network, visibility: "public" } })).toBe(true);
  });

  it("needs a role and a machine name to leave the first step", () => {
    expect(canContinue("setup-profile", { ...base, profile: null })).toBe(false);
    expect(canContinue("setup-profile", { ...base, profile: "admin", nodeName: " " })).toBe(false);
    expect(canContinue("setup-profile", { ...base, profile: "admin" })).toBe(true);
  });
});

describe("StartScreen", () => {
  it("only offers the roles the public network being joined accepts", () => {
    render(
      <LocaleProvider initialLocale="en">
        <StartScreen setup={{ ...base, profile: null, joinAccepts: ["infer"] }} hardware={null} update={() => undefined} t={(key) => key} />
      </LocaleProvider>,
    );
    const role = (name: string) => screen.getByRole("radio", { name: new RegExp(name) }) as HTMLButtonElement;
    expect(role("setupWizard.node").disabled).toBe(true);
    expect(role("setupWizard.infer").disabled).toBe(false);
    // Founding a network of one's own is never closed by the one being joined.
    expect(role("setupWizard.admin").disabled).toBe(false);
  });
});

describe("NetworkScreen", () => {
  function Harness() {
    const [setup, setSetup] = useState<SetupState>({ ...base, profile: "admin", advertiseAddr: "[2804::1]:42777", network: { ...base.network, public_name: "POM-1" } });
    return (
      <NetworkScreen
        setup={setup}
        hardware={null}
        countries={[{ code: "BR", name: "Brasil", cities: [{ id: "cwb", name: "Curitiba" }] }]}
        update={(patch) => setSetup((current) => ({ ...current, ...patch }))}
        updateNetwork={(patch) => setSetup((current) => ({ ...current, network: { ...current.network, ...patch } }))}
        t={t}
      />
    );
  }

  it("offers only private or public, and asks the location only for public", async () => {
    wrap(<Harness />);
    expect(screen.getAllByRole("radio")).toHaveLength(2);
    expect(screen.queryByText("setupWizard.accessPolicy")).toBeNull();
    expect(screen.queryByText("setupWizard.country")).toBeNull();
    fireEvent.click(screen.getByText("firstRun.public"));
    await waitFor(() => expect(screen.getByText("setupWizard.country")).toBeTruthy());
  });
});

describe("PublicDirectoryPanel", () => {
  const listed = {
    cluster_id: "curitiba",
    name: "Rede Curitiba",
    fingerprint: "fp",
    publisher_key: "pk:curitiba",
    trust_document: "{}",
    organization: "Equipe Sul",
    country_name: "Brasil",
    country_code: "BR",
    city: "Curitiba",
  };

  it("lists the directory in the wizard, with chat only where a lobby is open", async () => {
    const get = vi.spyOn(api, "get").mockResolvedValue([
      { ...listed, lobby_chat: true },
      { ...listed, cluster_id: "lisboa", name: "Rede Lisboa", fingerprint: "fp2", accepted_enrollments: [] },
    ] as never);
    const onUse = vi.fn();
    wrap(<PublicDirectoryPanel setup={{ ...base, profile: "node", publicDirectory: "https://dir.example" }} onUse={onUse} t={t} />);

    const open = (await screen.findByText("Rede Curitiba")).closest("div.flex")!;
    // The setup router has no session, and the typed replicas ride along.
    expect(get).toHaveBeenCalledWith("/api/setup/public-networks?replicas=https%3A%2F%2Fdir.example", { auth: false });
    expect(open.querySelectorAll("button").length).toBe(2);
    fireEvent.click(screen.getByRole("button", { name: "firstRun.publicDirectoryUse" }));
    expect(onUse).toHaveBeenCalledWith(expect.objectContaining({ cluster_id: "curitiba" }));

    const closed = screen.getByRole("button", { name: "publicNetworks.closed" }) as HTMLButtonElement;
    expect(closed.disabled).toBe(true);
    expect(screen.getAllByRole("button", { name: /publicNetworks.chat/ }).length).toBe(1);
  });
});

describe("local discovery", () => {
  it("shows each computer that searches the network with its name, system, IP, RAM and GPU", async () => {
    vi.spyOn(api, "post").mockResolvedValue({} as never);
    vi.spyOn(api, "get").mockResolvedValue({
      enabled: true,
      active: false,
      searching: false,
      found: false,
      port: 42778,
      peers: [
        {
          request_id: "r1",
          address: "192.168.1.42:42778",
          joined: false,
          machine: { hostname: "salem", os: "linux", arch: "x86_64", cpu_count: 16, ram_mb: 65536, gpus: [{ name: "RTX 3060", vram_mb: 12288 }] },
        },
        { request_id: "r2", address: "192.168.1.50:42778", joined: true, machine: { hostname: "macbook", os: "macos", arch: "aarch64", ram_mb: 16384, gpus: [] } },
      ],
    } as never);

    wrap(<LocalDiscoveryPanel mode="admin" setup={{ ...base, profile: "admin", trustDocument: "trust", connectionKey: "secret" }} t={t} />);

    await waitFor(() => expect(screen.getAllByTestId("discovered-machine")).toHaveLength(2));
    const [salem, mac] = screen.getAllByTestId("discovered-machine");
    expect(salem.textContent).toContain("salem");
    expect(salem.textContent).toContain("linux · x86_64");
    expect(salem.textContent).toContain("192.168.1.42");
    expect(salem.textContent).toContain("64 GB");
    expect(salem.textContent).toContain("RTX 3060 (12 GB)");
    expect(salem.textContent).toContain("firstRun.machineJoining");
    expect(mac.textContent).toContain("firstRun.machineJoined");
    expect(mac.textContent).toContain("setupWizard.noGpu");
  });

  it("fills the trust and the key when the administrator answers", async () => {
    const document = JSON.stringify({ anchor: { cluster_id: "POM-ABCDE" }, setup: { public_name: "Casa", visibility: "private" } });
    const post = vi.spyOn(api, "post").mockResolvedValue({} as never);
    vi.spyOn(api, "get").mockResolvedValue({
      enabled: true,
      active: false,
      searching: false,
      found: true,
      port: 42778,
      peers: [],
      result: { request_id: "r1", trust_document: document, trust_fingerprint: "ab".repeat(32), connection_key: "secret" },
    } as never);

    function Harness() {
      const [setup, setSetup] = useState<SetupState>({ ...base, profile: "node", nodeName: "salem" });
      return (
        <>
          <JoinScreen setup={setup} update={(patch) => setSetup((current) => ({ ...current, ...patch }))} updateNetwork={(patch) => setSetup((current) => ({ ...current, network: { ...current.network, ...patch } }))} t={t} />
          <span data-testid="ready">{String(isReadyToSave(setup))}</span>
        </>
      );
    }
    wrap(<Harness />);

    await waitFor(() => expect(screen.getByTestId("join-network").textContent).toContain("Casa"));
    expect(post).toHaveBeenCalledWith("/api/setup/auto-discovery/search", { node_name: "salem" }, { auth: false });
    expect(screen.getByTestId("ready").textContent).toBe("true");
  });
});

describe("TrustFields", () => {
  it("warns when the pasted trust document is not valid", async () => {
    function Harness() {
      const [setup, setSetup] = useState<SetupState>(base);
      const update = (patch: Partial<SetupState>) => setSetup((current) => ({ ...current, ...patch }));
      return <TrustFields setup={setup} update={update} applyTrust={(document) => update({ trustDocument: document })} t={t} />;
    }
    wrap(<Harness />);
    fireEvent.change(screen.getByTestId("trust-document-textarea"), { target: { value: "not json" } });
    await waitFor(() => expect(screen.getByTestId("trust-document-invalid")).toBeTruthy());
    expect(screen.getByTestId("connection-key-input")).toBeTruthy();
  });
});
