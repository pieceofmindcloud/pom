// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useCallback, useEffect, useState } from "react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { NetworkChat } from "./NetworkChat";

const mocks = vi.hoisted(() => ({
  get: vi.fn(),
  post: vi.fn(),
  toast: { error: vi.fn(), fromError: vi.fn(), success: vi.fn() },
  admissions: { data: [] as unknown[], refetch: vi.fn() },
  decide: vi.fn(),
}));

vi.mock("../lib/admissions", () => ({
  useAdmissions: () => mocks.admissions,
  admissionsApi: { decide: mocks.decide },
}));

vi.mock("../lib/api", () => ({
  api: { get: mocks.get },
  postNetworkChat: mocks.post,
}));
vi.mock("../lib/queries", () => ({
  useNode: () => ({ data: { node_id: "node-local" } }),
  useCluster: () => ({ data: { peers: [{ node_id: "node-peer", short_id: "node-peer", node: { hostname: "salem" } }] } }),
  useNetworkChat: () => {
    const [data, setData] = useState<unknown>(null);
    const [isLoading, setIsLoading] = useState(true);
    const refetch = useCallback(async () => {
      const next = await mocks.get();
      setData(next);
      setIsLoading(false);
      return { data: next };
    }, []);
    useEffect(() => {
      void refetch();
    }, [refetch]);
    return { data, isLoading, error: null, refetch };
  },
}));
vi.mock("../lib/toast", () => ({
  useToast: () => mocks.toast,
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

function mount(path = "/network-chat") {
  return render(
    <MemoryRouter initialEntries={[path]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale="pt-BR">
        <NetworkChat />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("NetworkChat", () => {
  it("loads the history, participants and sends text with the identified action", async () => {
    mount();

    expect(await screen.findByText("Olá da rede")).toBeTruthy();
    expect(screen.getAllByText("salem").length).toBeGreaterThan(1);
    expect(screen.queryByText("node-peer [09:00")).toBeNull();

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

  it("scrolls to the last message when the history receives a new message", async () => {
    const nextSnapshot = {
      ...enabledSnapshot,
      messages: [
        ...enabledSnapshot.messages,
        {
          ...enabledSnapshot.messages[0],
          messageId: "message-2",
          senderNodeId: "node-local",
          text: "Mensagem nova",
        },
      ],
    };
    mocks.get.mockReset().mockResolvedValueOnce(enabledSnapshot).mockResolvedValue(nextSnapshot);
    mount();

    const messages = await screen.findByTestId("network-chat-messages");
    const scrollTo = vi.fn();
    Object.defineProperties(messages, {
      clientHeight: { configurable: true, value: 240 },
      scrollHeight: { configurable: true, value: 900 },
      scrollTop: { configurable: true, writable: true, value: 660 },
      scrollTo: { configurable: true, value: scrollTo },
    });
    fireEvent.scroll(messages);

    const composer = screen.getByPlaceholderText("Escreva para a rede…");
    fireEvent.change(composer, { target: { value: "Mensagem nova" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));

    await waitFor(() => expect(screen.getByText("Mensagem nova")).toBeTruthy());
    expect(scrollTo).toHaveBeenCalledWith({ top: 900, behavior: "auto" });
  });

  it("sends text over HTTP without crypto.randomUUID", async () => {
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

  it("stays inert when the principal turns the channel off", async () => {
    mocks.get.mockResolvedValue({ ...enabledSnapshot, enabled: false });
    mount();

    expect(await screen.findByText("Chat da rede está desligado")).toBeTruthy();
    expect(screen.queryByPlaceholderText("Escreva para a rede…")).toBeNull();
  });

  it("keeps the participants hidden on mobile until the person asks for the list", async () => {
    mount();

    const toggle = await screen.findByRole("button", { name: "Ver online" });
    const participants = screen.getByRole("complementary").parentElement;
    expect(participants?.className).toContain("hidden");

    fireEvent.click(toggle);

    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(participants?.className).toContain("block");
  });

  it("opens each candidate's thread with its hardware and answers in it", async () => {
    const threadMessage = (candidate: string, text: string) => ({
      ...enabledSnapshot.messages[0],
      messageId: `m-${candidate}`,
      roomId: `lobby/${candidate}`,
      senderNodeId: candidate,
      text,
    });
    mocks.get.mockResolvedValue({
      ...enabledSnapshot,
      messages: [threadMessage("candidate-node-id", "Oi, sou o primeiro"), threadMessage("other-candidate", "Oi, sou o segundo")],
      lobbyEnabled: true,
      lobbyGuests: [{
        nodeId: "candidate-node-id",
        card: { hostname: "forasteiro", os: "linux", arch: "x86_64", cpu_count: 16, ram_total_mb: 65536, gpus: [{ name: "RTX 5060 Ti", vram_total_mb: 16384 }] },
      }],
    });
    mount();

    fireEvent.click(await screen.findByRole("tab", { name: /Lobby/ }));
    expect(await screen.findByTestId("lobby-card-candidate-node-id")).toBeTruthy();
    expect(screen.getAllByText("forasteiro").length).toBeGreaterThan(0);
    expect(screen.getByText("RTX 5060 Ti")).toBeTruthy();
    expect(screen.getByText("16 GB")).toBeTruthy();
    // One candidate's thread at a time: the other's words are not on screen.
    expect(screen.getByText("Oi, sou o primeiro")).toBeTruthy();
    expect(screen.queryByText("Oi, sou o segundo")).toBeNull();
    fireEvent.click(screen.getByTestId("lobby-thread-other-candidate"));
    expect(await screen.findByText("Oi, sou o segundo")).toBeTruthy();
    expect(screen.queryByText("Oi, sou o primeiro")).toBeNull();
    fireEvent.click(screen.getByTestId("lobby-thread-candidate-node-id"));
    // No file goes to a room that non-members read.
    expect(screen.queryByRole("button", { name: "Anexar arquivo" })).toBeNull();

    fireEvent.change(screen.getByPlaceholderText("Escreva para o lobby…"), { target: { value: "Bem-vindo" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    await waitFor(() => {
      expect(mocks.post).toHaveBeenCalledWith(expect.objectContaining({
        action: expect.objectContaining({ kind: "text", room_id: "lobby/candidate-node-id", text: "Bem-vindo" }),
      }));
    });
    // Typing is a signal of the members' room only.
    expect(mocks.post.mock.calls.every(([request]) => request.action.kind !== "typing")).toBe(true);
  });

  it("opens the thread the bell points at, marks it, and decides the admission inside it", async () => {
    mocks.admissions.data = [{
      node_id: "other-candidate", role: "worker", status: "pending", requested_at: "2026-10-05T12:00:00Z",
      card: { hostname: "segundo", os: "", arch: "", cpu_count: 0, ram_total_mb: 0, gpus: [] },
    }];
    mocks.decide.mockResolvedValue([]);
    mocks.get.mockResolvedValue({
      ...enabledSnapshot,
      messages: ["candidate-node-id", "other-candidate"].map((candidate) => ({
        ...enabledSnapshot.messages[0], messageId: `m-${candidate}`, roomId: `lobby/${candidate}`, senderNodeId: candidate, text: `Oi de ${candidate}`,
      })),
      lobbyEnabled: true,
      lobbyGuests: [],
    });
    mount("/network-chat?lobby=other-candidate");

    expect(await screen.findByText("Oi de other-candidate")).toBeTruthy();
    expect(screen.queryByText("Oi de candidate-node-id")).toBeNull();
    expect(screen.getByTestId("lobby-thread-other-candidate").getAttribute("aria-current")).toBe("true");
    expect(screen.getByTestId("lobby-thread-candidate-node-id").getAttribute("aria-current")).toBeNull();
    const header = screen.getByTestId("lobby-thread-header");
    expect(header.textContent).toContain("pede para entrar como");

    fireEvent.click(screen.getByRole("button", { name: "Aprovar" }));
    await waitFor(() => expect(mocks.decide).toHaveBeenCalledWith("other-candidate", "approve"));
    mocks.admissions.data = [];
  });

  it("shows no room switch while the network has no lobby", async () => {
    mount();
    expect(await screen.findByText("Olá da rede")).toBeTruthy();
    expect(screen.queryByRole("tab")).toBeNull();
  });
});
