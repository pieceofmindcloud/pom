// @vitest-environment jsdom
/**
 * "Esta máquina" and "Endereço anunciado" left NetworkSettings.tsx for
 * here, a menu screen of their own ("Minha Máquina"). See `NetworkSettings.test.tsx`
 * for the screen with the overall picture of the network.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { MyMachine } from "./MyMachine";
import { api } from "../lib/api";

const queryMocks = vi.hoisted(() => ({
  node: {
    data: {
      hostname: "infer-box",
      node_id: "a",
      p2p_port: 42777,
      gateway_port: 8080,
      inference_port: 8181,
      profile: "worker",
      authority_level: "primary" as "primary" | null,
      detached: false,
      node_name: null as string | null,
      advertise_addr: null,
      effective_address: "127.0.0.1:42777",
    },
  },
}));

vi.mock("../lib/guide", () => ({
  useGuideActions: () => ({ complete: vi.fn(async () => {}), dismiss: vi.fn(async () => {}) }),
}));
vi.mock("../lib/api", () => ({
  api: {
    post: vi.fn(async () => undefined),
    // The relay panel loads on its own and stays out without an answer.
    get: vi.fn(async () => { throw new Error("no relay status in this test"); }),
  },
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));
vi.mock("../lib/queries", () => ({
  useNode: () => queryMocks.node,
  useInvalidate: () => vi.fn(),
}));

function mount() {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <MyMachine />
    </LocaleProvider>,
  );
}

describe("MyMachine", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("shows the machine identity and the announced address", () => {
    mount();
    expect(screen.getByText("Esta máquina")).toBeTruthy();
    expect(screen.getByText("infer-box")).toBeTruthy();
    expect(screen.getByText("Endereço anunciado")).toBeTruthy();
    expect(screen.getByText("127.0.0.1:42777")).toBeTruthy();
  });

  it("lets the operator name the machine, which the setup wizard is no longer there to do", async () => {
    vi.mocked(api.post).mockResolvedValueOnce({ node_name: "Mesa do laboratório", hostname: "Mesa do laboratório" });
    mount();

    const input = screen.getByTestId("node-name") as HTMLInputElement;
    // Nothing chosen yet: the field is empty and shows the hostname in use.
    expect(input.value).toBe("");
    expect(input.placeholder).toBe("infer-box");

    fireEvent.change(input, { target: { value: "  Mesa do laboratório " } });
    fireEvent.click(screen.getByTestId("node-name-save"));

    await waitFor(() =>
      expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/network/node-name", {
        node_name: "Mesa do laboratório",
      }),
    );
    await waitFor(() => expect(input.value).toBe("Mesa do laboratório"));
  });

  it("keeps Leave network as the exit of the machine from the cluster", async () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Sair da rede" }));

    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/leave"));
    expect(vi.mocked(api.post)).not.toHaveBeenCalledWith("/api/ui/network/leave");
  });

  it("lets a node leave even without authority and without a loaded model", async () => {
    queryMocks.node.data.profile = "worker";
    queryMocks.node.data.authority_level = null;
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Sair da rede" }));

    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/leave"));
  });

  it("shows the transport mode the node reports and saves the switch at once", async () => {
    vi.mocked(api.get).mockImplementation(async (path: string) => {
      if (path === "/api/ui/network/transport") return { no_iroh: false, live: false, restart_needed: false };
      throw new Error("no relay status in this test");
    });
    vi.mocked(api.post).mockResolvedValueOnce({ no_iroh: true, live: true, restart_needed: false });
    mount();

    expect(await screen.findByText("Conexões")).toBeTruthy();
    const box = screen.getByTestId("transport-direct-only").querySelector("input") as HTMLInputElement;
    expect(box.checked).toBe(false);
    expect(screen.getByTestId("transport-state").textContent).toBe("iroh ligado: descoberta, relays e furos de NAT");

    fireEvent.click(box);
    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/network/transport", { no_iroh: true }));
    // The node answered with the mode live: the card and the one line follow it.
    await waitFor(() => expect(box.checked).toBe(true));
    expect(screen.getByTestId("transport-state").textContent).toBe("iroh desligado: só endereços diretos");
  });

  it("says when the saved mode still waits for the restart it needs", async () => {
    vi.mocked(api.get).mockImplementation(async (path: string) => {
      if (path === "/api/ui/network/transport") return { no_iroh: true, live: false, restart_needed: true };
      throw new Error("no relay status in this test");
    });
    mount();

    await screen.findByTestId("transport-direct-only");
    const box = screen.getByTestId("transport-direct-only").querySelector("input") as HTMLInputElement;
    // The switch is what is saved, the line is what the node is doing now.
    expect(box.checked).toBe(true);
    expect(screen.getByTestId("transport-state").textContent).toBe("Salvo: iroh desligado vale na próxima iniciação");
    expect(screen.getByText(/O transporte de uma execução é decidido quando o nó inicia/)).toBeTruthy();
  });
});
