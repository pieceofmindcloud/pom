// @vitest-environment jsdom
/**
 * Depois de uma pergunta no chat, o menu parava de responder: a URL do link
 * clicado entrava na barra de endereços e a tela continuava sendo o chat. Só um
 * F5 devolvia a navegação.
 *
 * A causa não estava no menu. Ao fim do stream o chat congela um retrato da
 * telemetria daquela execução, e o efeito que grava esse retrato comparava por
 * **referência** um mapa que nascia novo a cada render - `useQueries` devolve um
 * array novo toda vez. O efeito gravava estado, o estado disparava outro render,
 * e o React abortava a árvore com "Maximum update depth exceeded". Nada mais
 * commitava, inclusive a troca de rota.
 *
 * O teste percorre o caminho do usuário: abre `/chat`, envia uma pergunta, e
 * clica em "Painel" no menu. O que ele exige é a tela nova na tela.
 */
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider, type Profile } from "../lib/profile";
import { ThemeProvider } from "../lib/theme";
import { ToastProvider } from "../lib/toast";
import type { RequestTrace } from "../lib/chatStream";

const nodeData = {
  hostname: "salem",
  short_id: "a1b2",
  status: { state: "idle" },
  config: null,
  tls_required: false,
  build_commit: "abc1234",
};

/**
 * O `x_pom` que o nó manda no último chunk. Um salto basta: o laço nascia do
 * mapa de runtimes por nó, e um nó já monta o mapa.
 */
const trace: RequestTrace = {
  model: "qwen",
  input_tokens: 8,
  output_tokens: 2,
  prefill_ms: 10,
  total_bytes_sent: 100,
  total_bytes_received: 200,
  hops: [{ node_id: "abcdef0123456789", start_layer: 0, end_layer: 15, ms: 10, bytes_sent: 100, bytes_received: 200, calls: 1 } as never],
};

vi.mock("../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/api")>();
  return {
    ...actual,
    // eslint-disable-next-line require-yield
    streamSse: async function* () {
      yield JSON.stringify({ choices: [{ delta: { content: "Olá" } }] });
      yield JSON.stringify({ x_pom: { tokens_per_sec: 3, tokens_generated: 2, duration_ms: 700, trace } });
      yield "[DONE]";
    },
  };
});

/**
 * Um laço de render trava o worker para sempre - o defeito original queimava
 * 100% de CPU sem nunca devolver -, e teste que não termina não avisa ninguém.
 * O corte transforma a regressão numa falha legível.
 */
const loop = vi.hoisted(() => ({ renders: 0, limit: 400, tripped: false }));

/**
 * `usePeerRuntimes` devolve um array novo a cada chamada de propósito: é o que
 * `useQueries` faz de verdade, e é a instabilidade que o defeito explorava. Um
 * dublê que devolvesse sempre o mesmo array esconderia o defeito em vez de
 * prová-lo.
 */
vi.mock("../lib/queries", () => ({
  useAdminSurface: () => ({ data: "available", isLoading: false, isError: false }),
  useNode: () => ({ data: nodeData, isLoading: false, isError: false }),
  useCluster: () => ({ data: null }),
  useInvalidate: () => vi.fn(),
  useServableModels: () => ({ data: [{ id: "qwen" }] }),
  useInferenceHeads: () => ({ data: undefined, isLoading: false, isError: false }),
  useChatTelemetryNetwork: () => ({ data: undefined, isLoading: false, isError: false }),
  usePeerRuntimes: (nodeIds: string[]) => {
    if (++loop.renders > loop.limit) {
      loop.tripped = true;
      throw new Error(`laço de render: ${loop.limit} renders do chat sem estabilizar`);
    }
    return nodeIds.map(() => ({ data: undefined, isLoading: false }));
  },
  useJobs: () => ({ data: [] }),
  useOffers: () => ({ data: [] }),
  useTransfers: () => ({ data: [] }),
}));

vi.mock("../components/Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));
vi.mock("./Dashboard", () => ({ Dashboard: () => <div>tela-painel</div> }));

import App from "../App";

function mount(profile: Profile = "admin") {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <ProfileProvider initialProfile={profile}>
        <ThemeProvider>
          <ToastProvider>
            <MemoryRouter
              initialEntries={["/chat"]}
              future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
            >
              <App />
            </MemoryRouter>
          </ToastProvider>
        </ThemeProvider>
      </ProfileProvider>
    </LocaleProvider>,
  );
}

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  loop.renders = 0;
  loop.tripped = false;
});

describe("navegação depois do chat", () => {
  it("troca de tela ao clicar no menu depois de uma resposta", async () => {
    mount();

    const composer = await screen.findByRole("textbox");
    act(() => {
      fireEvent.change(composer, { target: { value: "oi" } });
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: /enviar/i }));
    });

    // A resposta chegou: o turno terminou e a telemetria foi congelada.
    await screen.findByText("Olá");

    act(() => {
      fireEvent.click(screen.getByRole("link", { name: "Painel" }));
    });

    await waitFor(() => expect(screen.getByText("tela-painel")).toBeTruthy());
    expect(loop.tripped).toBe(false);
  });

  it("não duplica o histórico quando o infer já o tem no menu principal", async () => {
    mount("infer");

    await screen.findByPlaceholderText("Escreva para a rede…");
    expect(screen.getAllByTestId("chat-session-list")).toHaveLength(1);
    expect(screen.getAllByRole("button", { name: "Conversa nova" })).toHaveLength(1);
    expect(screen.getByRole("link", { name: "Rede" })).toBeTruthy();
  });

  it("mostra o alvo de largada, identifica o formato e remove o anexo sem apagar o rascunho", async () => {
    mount();

    const composer = await screen.findByRole("textbox");
    const dropTarget = screen.getByRole("group", { name: "Campo de mensagem" });
    act(() => {
      fireEvent.change(composer, { target: { value: "texto antes do anexo" } });
      fireEvent.dragEnter(dropTarget, {
        dataTransfer: { types: ["Files"], files: [] },
      });
    });
    expect(screen.getByText("Solte uma imagem ou um ficheiro de texto")).toBeTruthy();

    const file = new File(["{\"ok\":true}"], "dados.json", { type: "application/json" });
    Object.defineProperty(file, "text", { value: async () => "{\"ok\":true}" });
    act(() => {
      fireEvent.drop(dropTarget, {
        dataTransfer: { types: ["Files"], files: [file] },
      });
    });

    expect(await screen.findByText("dados.json")).toBeTruthy();
    expect(screen.getByLabelText("Formato JSON")).toBeTruthy();
    expect(screen.queryByText("Solte uma imagem ou um ficheiro de texto")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Remover dados.json" }));
    expect(screen.queryByText("dados.json")).toBeNull();
    expect((composer as HTMLTextAreaElement).value).toBe("texto antes do anexo");
  });

  it("recusa uma largada que não seja imagem ou texto com mensagem legível", async () => {
    mount();

    const dropTarget = await screen.findByRole("group", { name: "Campo de mensagem" });
    const file = new File([new Uint8Array([0, 1, 2])], "dados.bin", { type: "application/octet-stream" });
    fireEvent.drop(dropTarget, {
      dataTransfer: { types: ["Files"], files: [file] },
    });

    expect(await screen.findByText("aceite imagens ou ficheiros de texto")).toBeTruthy();
  });

  it("recusa imagem largada quando o modelo não tem visão", async () => {
    mount();

    const dropTarget = await screen.findByRole("group", { name: "Campo de mensagem" });
    const file = new File(["fake"], "foto.png", { type: "image/png" });
    fireEvent.drop(dropTarget, {
      dataTransfer: { types: ["Files"], files: [file] },
    });

    expect(await screen.findByText("este modelo não tem visão")).toBeTruthy();
  });
});
