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
  vi.unstubAllGlobals();
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

  it("envia texto em HTTP sem crypto.randomUUID", async () => {
    vi.stubGlobal("crypto", {
      getRandomValues: (bytes: Uint8Array) => {
        bytes.fill(0xcd);
        return bytes;
      },
    });
    mount();

    const composer = await screen.findByPlaceholderText("Escreva para a rede…");
    fireEvent.change(composer, { target: { value: "Mensagem em HTTP" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));

    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith(expect.objectContaining({
        action: expect.objectContaining({
          kind: "text",
          text: "Mensagem em HTTP",
          message_id: expect.stringMatching(/^[0-9a-f]{32}$/),
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
