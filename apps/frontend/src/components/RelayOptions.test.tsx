// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { api } from "../lib/api";
import { ProfileProvider } from "../lib/profile";
import type { Profile } from "../lib/profile";
import { RelayOptions, relayStateKey, type RelayStatus } from "./RelayOptions";

const base: RelayStatus = {
  everyone: false,
  cluster: true,
  transfers: true,
  fixed: { everyone: false, no_cluster: false, no_transfers: false },
  reachable: true,
  serving: true,
  anonymous: false,
  anonymous_serving: false,
  anonymous_relays: 0,
};

vi.mock("../lib/api", () => ({ api: { get: vi.fn(), post: vi.fn() } }));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));

function mount(profile: Profile = "worker") {
  return render(
    <LocaleProvider initialLocale="en">
      <ProfileProvider initialProfile={profile}>
        <RelayOptions />
      </ProfileProvider>
    </LocaleProvider>,
  );
}

describe("RelayOptions", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("says in one line what the machine does with the options", () => {
    expect(relayStateKey(base)).toBe("network.relayStateCluster");
    expect(relayStateKey({ ...base, reachable: false })).toBe("network.relayStateUnreachable");
    expect(relayStateKey({ ...base, cluster: false })).toBe("network.relayStateOff");
    expect(relayStateKey({ ...base, everyone: true, reachable: false })).toBe("network.relayStateEveryone");
  });

  it("saves one option at a time and shows what the node answered", async () => {
    vi.mocked(api.get).mockResolvedValue(base);
    vi.mocked(api.post).mockResolvedValue({ ...base, transfers: false });
    mount();

    const transfers = (await screen.findByTestId("relay-transfers")) as HTMLInputElement;
    expect(transfers.checked).toBe(true);
    fireEvent.click(transfers);

    await waitFor(() =>
      expect(api.post).toHaveBeenCalledWith("/api/ui/network/relay", {
        everyone: false,
        cluster: true,
        transfers: false,
        anonymous: false,
      }),
    );
    await waitFor(() => expect((screen.getByTestId("relay-transfers") as HTMLInputElement).checked).toBe(false));
  });

  it("locks an option the command line fixed", async () => {
    vi.mocked(api.get).mockResolvedValue({
      ...base,
      everyone: true,
      fixed: { ...base.fixed, everyone: true },
    });
    mount();

    const everyone = (await screen.findByTestId("relay-everyone")) as HTMLInputElement;
    expect(everyone.checked).toBe(true);
    expect(everyone.disabled).toBe(true);
    expect(screen.getByText("Set on the command line for this run")).toBeTruthy();
    expect(screen.getByTestId("relay-state").textContent).toBe("Relaying for any network");
  });

  it("stays out when the node does not answer", async () => {
    vi.mocked(api.get).mockRejectedValue(new Error("offline"));
    const { container } = mount();
    await waitFor(() => expect(api.get).toHaveBeenCalled());
    expect(container.textContent).toBe("");
  });

  it("stays out behind a NAT, where the machine cannot relay for anyone", async () => {
    vi.mocked(api.get).mockResolvedValue({ ...base, reachable: false, serving: false });
    const { container } = mount();
    await waitFor(() => expect(api.get).toHaveBeenCalled());
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(container.textContent).toBe("");
    expect(screen.queryByTestId("relay-everyone")).toBeNull();
  });

  it("offers the anonymous relay to a visitor only", async () => {
    vi.mocked(api.get).mockResolvedValue(base);
    vi.mocked(api.post).mockResolvedValue({ ...base, anonymous: true });
    mount("visitor");

    const anonymous = (await screen.findByTestId("relay-anonymous")) as HTMLInputElement;
    fireEvent.click(anonymous);
    await waitFor(() =>
      expect(api.post).toHaveBeenCalledWith("/api/ui/network/relay", {
        everyone: false,
        cluster: true,
        transfers: true,
        anonymous: true,
      }),
    );

    cleanup();
    mount("worker");
    await screen.findByTestId("relay-transfers");
    expect(screen.queryByTestId("relay-anonymous")).toBeNull();
  });

  it("tells an admin behind a NAT how many visitors relay anonymously", async () => {
    vi.mocked(api.get).mockResolvedValue({ ...base, reachable: false, anonymous_relays: 2 });
    mount("admin");
    expect((await screen.findByTestId("relay-visitors")).textContent).toContain("2 visitors");
    expect(screen.queryByTestId("relay-choices")).toBeNull();
  });
});
