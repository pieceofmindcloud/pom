// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import React from "react";
import { useState } from "react";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider } from "../lib/toast";
import { api } from "../lib/api";
import { AutoDiscoveryScreen, canContinue, createRandomNetworkName, diskSharePercent, formatAdvertiseAddress, InviteScreen, isReadyToSave, NetworkScreen, ProfileScreen, ReviewScreen, TrustScreen, type SetupState } from "./Setup";

const initialSetup: SetupState = {
  profile: "infer",
  bootstrap: "",
  publicDirectory: "",
  connectionKey: "",
  advertiseAddr: "",
  trustDocument: "",
  trustFingerprint: "",
  adminAccessToken: "",
  adminExposure: "localhost",
  tlsRequired: false,
  nodeName: "node",
  network: {
    public_name: "",
    organization: "",
    description: "",
    rules: "",
    channels: "",
    visibility: "private",
    access_policy: "authorized",
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

function mount(value: SetupState = initialSetup) {
  function Harness() {
    const [setup, setSetup] = useState(value);
    return (
      <TrustScreen
        setup={setup}
        update={(patch) => setSetup((current) => ({ ...current, ...patch }))}
        updateNetwork={(patch) => setSetup((current) => ({ ...current, network: { ...current.network, ...patch } }))}
        t={(key) => key}
      />
    );
  }

  return render(
    <LocaleProvider initialLocale="pt-BR">
      <Harness />
    </LocaleProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("admin local discovery feedback", () => {
  it("shows each peer after its handshake completes", async () => {
    vi.spyOn(api, "post").mockResolvedValue({} as never);
    vi.spyOn(api, "get").mockResolvedValue({
      enabled: true,
      active: true,
      searching: false,
      found: true,
      port: 42778,
      peers: [{ request_id: "request-1", address: "192.168.1.42:42778" }],
    } as never);

    render(
      <LocaleProvider initialLocale="pt-BR">
        <ToastProvider>
          <AutoDiscoveryScreen
            setup={{ ...initialSetup, profile: "admin", trustDocument: "trust", connectionKey: "secret" }}
            update={() => {}}
            updateNetwork={() => {}}
            t={(key) => key}
          />
        </ToastProvider>
      </LocaleProvider>,
    );

    await waitFor(() => expect(screen.getByText("192.168.1.42:42778")).toBeTruthy());
    expect(screen.getByText("setupWizard.discoveryPeerCount")).toBeTruthy();
  });
});

describe("TrustScreen", () => {
  it("aceita trust.json por upload sem pedir confirmação manual", async () => {
    mount();
    const input = screen.getByTestId("trust-document-file") as HTMLInputElement;
    const document = JSON.stringify({ anchor: { principal: { pubkey: "admin-key" } } });

    fireEvent.change(input, {
      target: { files: [new File([document], "trust.json", { type: "application/json" })] },
    });

    expect(await screen.findByDisplayValue(document)).toBeTruthy();
    expect(screen.queryByRole("checkbox")).toBeNull();
  });

  it("pede somente a raiz e a chave ao node", () => {
    mount({ ...initialSetup, profile: "node" });

    expect(screen.getByTestId("trust-document-file")).toBeTruthy();
    expect(screen.getByTestId("connection-key-input")).toBeTruthy();
    expect(screen.queryByTestId("public-network-name")).toBeNull();
    expect(screen.queryByTestId("public-directory-trust")).toBeNull();
    expect(screen.queryByLabelText("setupWizard.bootstrapAddress")).toBeNull();
  });

  it("não pede identidade da rede para liberar o último passo do node", () => {
    const setup: SetupState = {
      ...initialSetup,
      profile: "node",
      trustDocument: "{}",
      trustFingerprint: "fingerprint",
      connectionKey: "secret",
    };

    expect(canContinue("setup-trust", setup)).toBe(true);
    expect(isReadyToSave(setup)).toBe(true);
  });
});

describe("first-page defaults", () => {
  it("shows the shared-disk slider on the first machine screen", () => {
    render(
      <ProfileScreen
        profile="admin"
        setup={initialSetup}
        hardware={{
          system: {
            hostname: "machine",
            os: "macos",
            arch: "arm64",
            cpu_count: 10,
            ram: { total_mb: 16384, available_mb: 8192 },
            gpu_kind: "integrated",
            gpus: [],
            disk: { total_mb: 512000, free_mb: 256000 },
          },
          gpu_detected: false,
          cuda_available: false,
          vulkan_available: false,
          backend: "Metal",
          p2p_discovery_available: true,
          public_ipv6_available: true,
          p2p_port: 45123,
          default_model_dir: "/models",
          default_shard_dir: "/models_sliced",
          shard_max_gb: 20,
          ram_reserve_pct: 20,
          cpu_reserve_pct: 20,
          model_dirs: [],
          shard_dir: "/models_sliced",
          node_name: "machine",
          network: initialSetup.network,
          public_directory: [],
        }}
        onSelect={() => {}}
        update={() => {}}
        t={(key) => key}
      />,
    );

    expect(screen.getByTestId("disk-share-slider")).toBeTruthy();
    expect(screen.getByText("setupWizard.shardSpace")).toBeTruthy();
    expect(screen.getByText("20 GB")).toBeTruthy();
  });

  it("requires the admin IPv6 before leaving the machine screen", () => {
    expect(canContinue("setup-profile", { ...initialSetup, profile: "admin" })).toBe(false);
    expect(canContinue("setup-profile", { ...initialSetup, profile: "admin", advertiseAddr: "[2001:db8::10]:42777" })).toBe(true);
    expect(canContinue("setup-profile", { ...initialSetup, profile: "node" })).toBe(true);
  });

  it("creates a short editable public network name", () => {
    expect(createRandomNetworkName(0.42)).toMatch(/^POM-[A-Z0-9]{5}$/);
  });

  it("represents shared disk space as a bounded fraction of free space", () => {
    expect(diskSharePercent(20, 100)).toBe(20);
    expect(diskSharePercent(200, 100)).toBe(100);
    expect(diskSharePercent(-1, 100)).toBe(0);
    expect(diskSharePercent(20, 0)).toBe(0);
  });

  it("formats an IPv6 address with the configured P2P port", () => {
    expect(formatAdvertiseAddress("2001:db8::10", 45123)).toBe("[2001:db8::10]:45123");
    expect(formatAdvertiseAddress("[2001:db8::10]:45123", 42777)).toBe("[2001:db8::10]:45123");
    expect(formatAdvertiseAddress("", 45123)).toBe("");
  });

  it("does not require or render manual directory replicas for a public admin network", () => {
    const setup: SetupState = {
      ...initialSetup,
      profile: "admin",
      advertiseAddr: "[2001:db8::10]:42777",
      trustDocument: "trust",
      trustFingerprint: "fingerprint",
      network: {
        ...initialSetup.network,
        public_name: "POM-PUBLIC",
        visibility: "public",
        country_code: "BR",
        city: "Curitiba",
      },
    };

    expect(canContinue("setup-network", setup)).toBe(true);
    expect(isReadyToSave(setup)).toBe(true);

    render(<NetworkScreen setup={setup} updateNetwork={() => {}} t={(key) => key} />);
    expect(screen.queryByTestId("public-directory")).toBeNull();
  });
});

describe("merged review", () => {
  it("keeps the health checks and final summary in one screen", () => {
    render(
      <ReviewScreen
        setup={{ ...initialSetup, profile: "node", trustFingerprint: "fingerprint" }}
        health={{
          gpu_detected: false,
          cuda_working: false,
          vulkan_working: false,
          configuration_ready: true,
          p2p_discovery_working: true,
          public_ipv6_enabled: true,
          firewall_allowed: true,
          firewall_supported: false,
        }}
        hardware={null}
        setStage={() => {}}
        t={(key) => key}
      />,
    );

    expect(screen.getByText("setupWizard.healthTitle")).toBeTruthy();
    expect(screen.getByText("setupWizard.checkIpv6")).toBeTruthy();
    expect(screen.getAllByText("setupWizard.reviewTitle")).toHaveLength(2);
    expect(screen.getByText("setupWizard.trustLabel")).toBeTruthy();
  });
});

describe("network invitation", () => {
  it("prioritizes the connection secret and collapses long materials", () => {
    const view = render(
      <InviteScreen
        anchor={{
          cluster_id: "cluster",
          document: '{"setup":{"public_name":"POM"}}',
          fingerprint: "fingerprint",
          trust_filename: "trust.json",
        }}
        visibility="private"
        connectionKey="connection-secret"
        t={(key) => key}
      />,
    );

    const details = view.container.querySelectorAll("details");
    expect(details).toHaveLength(1);
    expect(Array.from(details).every((detail) => !detail.open)).toBe(true);
    expect(screen.queryByText("setupWizard.publicKey")).toBeNull();
    expect(screen.getByText("connection-secret")).toBeTruthy();
    expect(screen.getByRole("button", { name: "setupWizard.download" })).toBeTruthy();

    fireEvent.click(screen.getAllByText("setupWizard.showContents")[0]);
    expect(details[0].open).toBe(true);
    expect(screen.getByText("setupWizard.hideContents")).toBeTruthy();
  });
});
