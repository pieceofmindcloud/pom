// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { NetworkChat } from "./NetworkChat";

const mocks = vi.hoisted(() => ({
  get: vi.fn(),
  post: vi.fn(),
}));

vi.mock("../lib/api", () => ({
  api: { get: mocks.get },
  postNetworkChat: mocks.post,
}));
vi.mock("../lib/queries", () => ({
  useNode: () => ({ data: { node_id: "node-local" } }),
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ error: vi.fn(), fromError: vi.fn() }),
}));

const enabledSnapshot = {
  enabled: true,
  messages: [
    {
      messageId: "message-1",
      roomId: "network",
      senderNodeId: "node-peer",
      recipientNodeId: null,
      kind: "text",
      text: "Olá da rede",
      fileName: null,
      mimeType: null,
      fileData: [],
      sentAt: "2026-09-09T12:00:00Z",
    },
  ],
  participants: [{ nodeId: "node-peer", hostname: "salem", online: true, profile: "Node" }],
  typing: [],
  dropped: 0,
};

beforeEach(() => {
  mocks.get.mockResolvedValue(enabledSnapshot);
  mocks.post.mockResolvedValue({ accepted: true });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function mount() {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <NetworkChat />
    </LocaleProvider>,
  );
}

describe("NetworkChat", () => {
  it("carrega o histórico, participantes e envia texto com a ação identificada", async () => {
    mount();

    expect(await screen.findByText("Olá da rede")).toBeTruthy();
    expect(screen.getByText("salem")).toBeTruthy();

    const composer = screen.getByPlaceholderText("Escreva para a rede…");
    fireEvent.change(composer, { target: { value: "Mensagem nova" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));

    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith(expect.objectContaining({
        action: expect.objectContaining({
          kind: "text",
          room_id: "network",
          sender_node_id: "node-local",
          text: "Mensagem nova",
        }),
      }));
    });
  });

  it("fica inerte quando o principal desliga o canal", async () => {
    mocks.get.mockResolvedValue({ ...enabledSnapshot, enabled: false });
    mount();

    expect(await screen.findByText("Chat da rede está desligado")).toBeTruthy();
    expect(screen.queryByPlaceholderText("Escreva para a rede…")).toBeNull();
  });
});
