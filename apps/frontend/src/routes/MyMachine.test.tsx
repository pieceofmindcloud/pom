// @vitest-environment jsdom
/**
 * "Esta máquina" e "Endereço anunciado" saíram de NetworkSettings.tsx para
 * cá, numa tela própria de menu ("Minha Máquina"). Ver `NetworkSettings.test.tsx`
 * para a tela de retrato geral da rede.
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
      profile: "node",
      authority_level: "primary" as "primary" | null,
      detached: false,
      advertise_addr: null,
      effective_address: "127.0.0.1:42777",
    },
  },
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

  it("mostra a identidade da máquina e o endereço anunciado", () => {
    mount();
    expect(screen.getByText("Esta máquina")).toBeTruthy();
    expect(screen.getByText("infer-box")).toBeTruthy();
    expect(screen.getByText("Endereço anunciado")).toBeTruthy();
    expect(screen.getByText("127.0.0.1:42777")).toBeTruthy();
  });

  it("mantém Sair da rede como saída da máquina do cluster", async () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Sair da rede" }));

    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/leave"));
    expect(vi.mocked(api.post)).not.toHaveBeenCalledWith("/api/ui/network/leave");
  });

  it("deixa um node sair mesmo sem autoridade e sem modelo carregado", async () => {
    queryMocks.node.data.profile = "node";
    queryMocks.node.data.authority_level = null;
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Sair da rede" }));

    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/leave"));
  });
});
