// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { LobbyChat } from "./LobbyChat";

const mocks = vi.hoisted(() => ({ get: vi.fn(), post: vi.fn() }));

vi.mock("../lib/api", () => ({
  api: { get: mocks.get, post: mocks.post },
}));

const card = { hostname: "minha-maquina", os: "linux", arch: "x86_64", cpu_count: 8, ram_total_mb: 32768, gpus: [] };
const exchange = {
  accepted: true,
  error: null,
  node_id: "me",
  card,
  messages: [
    { message_id: "m1", sender_node_id: "admin", sender_name: "principal", member: true, text: "Quem é você?", sent_at: "2026-10-04T12:00:00Z" },
    { message_id: "m2", sender_node_id: "me", sender_name: "minha-maquina", member: false, text: "Quero entrar", sent_at: "2026-10-04T12:01:00Z" },
  ],
};
const network = { name: "Rede Curitiba", publisher_key: "pk:curitiba" };

function mount(endpoint: "/api/ui/network-chat/lobby" | "/api/setup/lobby" = "/api/ui/network-chat/lobby") {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <LobbyChat network={network} endpoint={endpoint} onClose={() => undefined} />
    </LocaleProvider>,
  );
}

beforeEach(() => {
  mocks.get.mockResolvedValue(exchange);
  mocks.post.mockResolvedValue(exchange);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("LobbyChat", () => {
  it("reads the lobby by the publisher key and shows who is a member", async () => {
    mount();

    expect(await screen.findByText("Quem é você?")).toBeTruthy();
    expect(mocks.get).toHaveBeenCalledWith("/api/ui/network-chat/lobby?key=pk%3Acuritiba", { auth: true });
    expect(screen.getByText("membro")).toBeTruthy();
    expect(screen.getByText("candidato")).toBeTruthy();
    // The card the network sees is this machine's own.
    expect(screen.getByTestId("lobby-card-me")).toBeTruthy();
    expect(screen.getByText("32 GB de RAM")).toBeTruthy();
    expect(screen.getByText("Nenhuma GPU informada")).toBeTruthy();
  });

  it("posts through the node, never to the other network from the browser", async () => {
    mount();
    await screen.findByText("Quem é você?");

    fireEvent.change(screen.getByLabelText("Escreva para o lobby desta rede…"), { target: { value: "Tenho 16 GB de VRAM" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));

    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith(
        "/api/ui/network-chat/lobby",
        expect.objectContaining({ key: "pk:curitiba", text: "Tenho 16 GB de VRAM", message_id: expect.any(String) }),
        { auth: true },
      );
    });
  });

  it("uses the setup router without a session in the wizard", async () => {
    mount("/api/setup/lobby");
    await screen.findByText("Quem é você?");
    expect(mocks.get).toHaveBeenCalledWith("/api/setup/lobby?key=pk%3Acuritiba", { auth: false });
  });

  it("says why when the network refuses", async () => {
    mocks.get.mockResolvedValue({ ...exchange, accepted: false, error: "lobby_disabled", messages: [] });
    mount();
    expect(await screen.findByText("Esta rede não pode ser alcançada para admissão no momento.")).toBeTruthy();
    // Nothing goes through a lobby that is off: only the notice, no message field and no request.
    expect(screen.getByTestId("lobby-disabled")).toBeTruthy();
    expect(screen.queryByLabelText("Escreva para o lobby desta rede…")).toBeNull();
    expect(screen.queryByTestId("lobby-request")).toBeNull();
  });

  it("shows only the request side when the network has its conversation off", async () => {
    mocks.get.mockResolvedValue({ ...exchange, messages: [], messages_closed: true });
    mount();
    expect(await screen.findByTestId("lobby-admission")).toBeTruthy();
    expect(screen.getByTestId("lobby-request")).toBeTruthy();
    expect(screen.queryByTestId("lobby-messages")).toBeNull();
    expect(screen.queryByLabelText("Escreva para o lobby desta rede…")).toBeNull();
  });

  it("offers to switch at once to a network that already admitted this machine", async () => {
    mocks.get.mockResolvedValue({
      ...exchange,
      admission: { key: "pk:curitiba", name: "Rede Curitiba", role: "worker", status: "approved", standing: true },
    });
    mocks.post.mockResolvedValue({ joined: true });
    mount();

    expect(await screen.findByText(/já foi admitida nesta rede como Nós da rede/)).toBeTruthy();
    // No request to make and nothing to wait for.
    expect(screen.queryByRole("button", { name: "Pedir admissão" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Entrar agora" })).toBeNull();

    // One click: no second confirmation in between.
    fireEvent.click(screen.getByRole("button", { name: "Trocar para esta rede" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/network/join", { key: "pk:curitiba" }));
    expect(screen.queryByRole("dialog", { name: /Entrar em/ })).toBeNull();
  });

  it("asks for confirmation when the approval answers a request just made", async () => {
    mocks.get.mockResolvedValue({
      ...exchange,
      admission: { key: "pk:curitiba", name: "Rede Curitiba", role: "worker", status: "approved", standing: false },
    });
    mount();

    fireEvent.click(await screen.findByRole("button", { name: "Entrar agora" }));
    expect(await screen.findByRole("dialog", { name: "Entrar em Rede Curitiba?" })).toBeTruthy();
    expect(mocks.post).not.toHaveBeenCalled();
  });

  it("tells a declined machine how long to wait, then lets it ask again", async () => {
    const declined = (retryAt: number | null) => ({
      ...exchange,
      admission: { key: "pk:curitiba", name: "Rede Curitiba", role: "worker", status: "rejected", retry_at: retryAt },
    });
    // 41 minutes and a few seconds left: rounded up, never down to "now".
    mocks.get.mockResolvedValue(declined(Math.floor(Date.now() / 1000) + 41 * 60 + 5));
    const first = mount();
    expect((await screen.findByTestId("lobby-admission-wait")).textContent).toContain("pedir de novo em 42 minutos");
    expect(screen.queryByRole("button", { name: "Pedir admissão" })).toBeNull();
    first.unmount();

    // The wait is over: the request form is back.
    mocks.get.mockResolvedValue(declined(Math.floor(Date.now() / 1000) - 10));
    mount();
    expect(await screen.findByRole("button", { name: "Pedir admissão" })).toBeTruthy();
    expect(screen.queryByTestId("lobby-admission-wait")).toBeNull();
    expect(screen.getByText(/Você já pode pedir de novo/)).toBeTruthy();
  });
});
