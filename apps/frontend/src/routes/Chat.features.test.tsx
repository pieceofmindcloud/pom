// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Bubble, Chat } from "./Chat";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";
import { ThemeProvider } from "../lib/theme";
import { ToastProvider } from "../lib/toast";
import { ChatSessionProvider, type ChatMessage } from "../lib/chatStore";
import type { InferenceHeadsResponse } from "../lib/types";

const streamScenario = vi.hoisted(() => ({
  release: null as (() => void) | null,
  signalAborted: false,
  lastBody: undefined as Record<string, unknown> | undefined,
}));

const headQuery = vi.hoisted(() => ({
  data: undefined as InferenceHeadsResponse | undefined,
  isLoading: false,
  isError: false,
}));

const thinkingScenario = vi.hoisted(() => ({ enabled: true }));
const ragScenario = vi.hoisted(() => ({ available: false, collections: [] as string[] }));

const imageResizeScenario = vi.hoisted(() => ({
  calls: [] as number[],
  reject: false,
}));

vi.mock("../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/api")>();
  return {
    ...actual,
    // eslint-disable-next-line require-yield
    streamSse: async function* (_path: string, body: unknown, signal: AbortSignal) {
      streamScenario.lastBody = body as Record<string, unknown>;
      yield JSON.stringify({ choices: [{ delta: { content: "primeiro" } }] });
      if (streamScenario.release) {
        await new Promise<void>((resolve) => {
          const release = streamScenario.release;
          streamScenario.release = () => {
            release?.();
            resolve();
          };
        });
      }
      streamScenario.signalAborted = signal.aborted;
      yield JSON.stringify({ choices: [{ delta: { content: " segundo" } }] });
      yield "[DONE]";
    },
  };
});

vi.mock("../lib/queries", () => ({
  useNode: () => ({
    data: {
      active_model: "qwen",
      config: { n_ctx: 4, max_new_tokens: 64 },
    },
    isLoading: false,
    isError: false,
  }),
  useCluster: () => ({ data: null }),
  useChatCapabilities: () => ({
    data: {
      vision: "supported",
      coverage: "complete",
      thinking: thinkingScenario.enabled ? "supported" : "unsupported",
      rag: ragScenario.available ? "supported" : "unavailable",
      rag_collections: ragScenario.collections,
    },
  }),
  useServableModels: () => ({ data: [{ id: "qwen", context_length: 4, max_tokens: 3 }] }),
  useInferenceHeads: () => headQuery,
  useChatTelemetryNetwork: () => ({ data: undefined, isLoading: false, isError: false }),
  usePeerRuntimes: () => [],
  usePluginUi: () => ({ data: { plugins: [] }, isLoading: false, isError: false }),
}));

vi.mock("../lib/imageResize", () => {
  class ImageResizeLimitError extends Error {}
  return {
    imageBudgetBytes: (contextTokens: number, usedBytes = 0) => Math.max(0, Math.floor(contextTokens * 0.9) * 4 - usedBytes),
    ImageResizeLimitError,
    resizeImageForVision: async (_file: File, maxBytes?: number) => {
      imageResizeScenario.calls.push(maxBytes ?? 0);
      if (imageResizeScenario.reject) throw new ImageResizeLimitError();
      return {
        data: "data:image/jpeg;base64,ZmFrZQ==",
        size: 4,
        width: 512,
        height: 256,
        resized: true,
      };
    },
  };
});

vi.mock("../components/Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));

function mountChat() {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <ProfileProvider initialProfile="admin">
        <ThemeProvider>
          <ToastProvider>
            <ChatSessionProvider>
              <Chat />
            </ChatSessionProvider>
          </ToastProvider>
        </ThemeProvider>
      </ProfileProvider>
    </LocaleProvider>,
  );
}

function resetBrowserSpeech() {
  Object.defineProperty(window, "SpeechRecognition", { configurable: true, value: undefined });
  Object.defineProperty(window, "webkitSpeechRecognition", { configurable: true, value: undefined });
}

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  resetBrowserSpeech();
  streamScenario.release = null;
  streamScenario.signalAborted = false;
  streamScenario.lastBody = undefined;
  imageResizeScenario.calls = [];
  imageResizeScenario.reject = false;
  thinkingScenario.enabled = true;
  ragScenario.available = false;
  ragScenario.collections = [];
  headQuery.data = undefined;
  headQuery.isLoading = false;
  headQuery.isError = false;
});

describe("Chat", () => {
  it("opens an empty conversation on the POM mark with the composer under it, and moves it down on the first message", async () => {
    mountChat();

    const empty = await screen.findByTestId("chat-empty");
    expect(empty.querySelector('[data-testid="chat-hero"] img[alt="POM"]')).toBeTruthy();
    expect(screen.getByTestId("chat-hero-phrase").getAttribute("aria-label")).toBe("PIECE OF MIND");
    // The composer sits inside the opening screen, not in a bottom bar.
    expect(empty.contains(screen.getByRole("textbox"))).toBe(true);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "oi" } });
    fireEvent.click(screen.getByRole("button", { name: /enviar/i }));

    await screen.findByText("primeiro segundo");
    expect(screen.queryByTestId("chat-empty")).toBeNull();
    expect(screen.queryByTestId("chat-hero")).toBeNull();
    expect(screen.getByRole("textbox")).toBeTruthy();
  });

  it("keeps a finished answer's thinking closed, with its token count, until asked", () => {
    const message: ChatMessage = {
      id: "a1",
      role: "assistant",
      content: "Resposta",
      reasoning: "passo um, passo dois",
      reasoningTokens: 3,
      at: new Date().toISOString(),
    };
    render(
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile="admin">
          <Bubble message={message} streaming={false} />
        </ProfileProvider>
      </LocaleProvider>,
    );

    const toggle = screen.getByRole("button", { name: /Pensou/ });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.getByTestId("chat-thinking-tokens").textContent).toBe("3 tokens");
    expect(screen.queryByText("passo um, passo dois")).toBeNull();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("passo um, passo dois")).toBeTruthy();
  });

  it("says the model is still thinking while no answer text arrived", () => {
    const message: ChatMessage = { id: "a2", role: "assistant", content: "", reasoning: "hmm", reasoningTokens: 1, at: new Date().toISOString() };
    render(
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile="admin">
          <Bubble message={message} streaming />
        </ProfileProvider>
      </LocaleProvider>,
    );
    expect(screen.getByRole("button", { name: /Pensando/ }).getAttribute("aria-expanded")).toBe("false");
    expect(screen.getByTestId("chat-thinking-tokens").textContent).toBe("1 token");
    expect(screen.queryByText("hmm")).toBeNull();
  });

  it("shows the head choice in the admin profile too when there are two candidates", () => {
    headQuery.data = {
      model: "qwen",
      status: "selected",
      selected_node_id: "ksia",
      selection_policy: "coverage_then_weighted_rtt_load",
      rtt_valid_for_secs: 300,
      candidates: [
        {
          node_id: "ksia",
          short_id: "ksia",
          hostname: "ksia",
          start_layer: 0,
          end_layer: 39,
          total_layers: 40,
          load: 0,
          runtime_capacity: { n_ctx: 8192, max_new_tokens: 256 },
          rtt_ms: 10,
          rtt_age_ms: 100,
          rtt_state: "fresh",
          selected: true,
        },
        {
          node_id: "salem",
          short_id: "salem",
          hostname: "salem",
          start_layer: 0,
          end_layer: 39,
          total_layers: 40,
          load: 0,
          runtime_capacity: { n_ctx: 8192, max_new_tokens: 256 },
          rtt_ms: 20,
          rtt_age_ms: 100,
          rtt_state: "fresh",
          selected: false,
        },
      ],
    };

    mountChat();

    expect(screen.getByRole("combobox", { name: "Cabeça de inferência" })).toBeTruthy();
  });

  it("sends the chosen head in the Chat request", async () => {
    headQuery.data = {
      model: "qwen",
      status: "selected",
      selected_node_id: "ksia",
      selection_policy: "coverage_then_weighted_rtt_load",
      rtt_valid_for_secs: 300,
      candidates: [
        {
          node_id: "ksia",
          short_id: "ksia",
          hostname: "ksia",
          start_layer: 0,
          end_layer: 39,
          total_layers: 40,
          load: 0,
          runtime_capacity: null,
          rtt_ms: 10,
          rtt_age_ms: 100,
          rtt_state: "fresh",
          selected: true,
        },
        {
          node_id: "salem",
          short_id: "salem",
          hostname: "salem",
          start_layer: 0,
          end_layer: 39,
          total_layers: 40,
          load: 0,
          runtime_capacity: null,
          rtt_ms: 20,
          rtt_age_ms: 100,
          rtt_state: "fresh",
          selected: false,
        },
      ],
    };

    mountChat();
    fireEvent.change(screen.getByRole("combobox", { name: "Cabeça de inferência" }), {
      target: { value: "salem" },
    });
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "oi" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));

    await waitFor(() => expect(streamScenario.lastBody?.inference_head).toBe("salem"));
  });

  it("asks for the output capacity of the model, not the size of one deployment leg", async () => {
    mountChat();
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "oi" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    // /v1/models diz 3; o max_new_tokens da implantação (64) é só a perna.
    await waitFor(() => expect(streamScenario.lastBody?.max_tokens).toBe(3));
  });

  it("sends the Think toggle per inference and keeps the choice in the conversation", async () => {
    mountChat();
    const think = screen.getByRole("button", { name: "Think" });
    expect(think.getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(think);
    expect(think.getAttribute("aria-pressed")).toBe("false");
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "primeira" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    await waitFor(() => expect(streamScenario.lastBody?.thinking_enabled).toBe(false));

    await waitFor(() => expect(screen.getByText(/segundo/)).toBeTruthy());
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "segunda" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    await waitFor(() => expect(streamScenario.lastBody?.thinking_enabled).toBe(false));
  });

  it("offers RAG in an icon with a menu, off until the person chooses", async () => {
    ragScenario.available = true;
    ragScenario.collections = ["folha", "manuais"];
    mountChat();
    const rag = screen.getByRole("button", { name: "RAG" });
    expect(rag.getAttribute("aria-pressed")).toBe("false");
    expect(screen.queryByRole("menu")).toBeNull();

    // Desligado é o padrão, e vai dito no pedido: o gateway não busca nada.
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "primeira" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    await waitFor(() => expect(streamScenario.lastBody?.rag).toBe(false));
    await waitFor(() => expect(screen.getByText(/segundo/)).toBeTruthy());

    fireEvent.click(screen.getByRole("button", { name: "RAG" }));
    const items = screen.getAllByRole("menuitemradio").map((item) => item.textContent);
    expect(items).toEqual(["Sem busca em documentos", "Todas as coleções", "folha", "manuais"]);
    expect(screen.getByRole("menuitemradio", { name: "Sem busca em documentos" }).getAttribute("aria-checked")).toBe("true");
    fireEvent.click(screen.getByRole("menuitemradio", { name: "Todas as coleções" }));
    expect(screen.queryByRole("menu")).toBeNull();
    expect(screen.getByRole("button", { name: "RAG" }).getAttribute("aria-pressed")).toBe("true");
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "segunda" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    await waitFor(() => expect(streamScenario.lastBody?.rag).toBe(true));
    expect(streamScenario.lastBody && "rag_collection" in streamScenario.lastBody).toBe(false);
  });

  it("limits the search to the collection chosen in the menu and turns off with the first option", async () => {
    ragScenario.available = true;
    ragScenario.collections = ["folha", "manuais"];
    mountChat();
    fireEvent.click(screen.getByRole("button", { name: "RAG" }));
    fireEvent.click(screen.getByRole("menuitemradio", { name: "folha" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "primeira" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    await waitFor(() => expect(streamScenario.lastBody?.rag).toBe(true));
    expect(streamScenario.lastBody?.rag_collection).toBe("folha");
    await waitFor(() => expect(screen.getByText(/segundo/)).toBeTruthy());

    fireEvent.click(screen.getByRole("button", { name: "RAG" }));
    expect(screen.getByRole("menuitemradio", { name: "folha" }).getAttribute("aria-checked")).toBe("true");
    fireEvent.click(screen.getByRole("menuitemradio", { name: "Sem busca em documentos" }));
    expect(screen.getByRole("button", { name: "RAG" }).getAttribute("aria-pressed")).toBe("false");
  });

  it("does not offer dictation on a page without HTTPS, where the browser denies the microphone", () => {
    // The recognizer exists; only the page is insecure.
    vi.stubGlobal("SpeechRecognition", class {});
    const secure = Object.getOwnPropertyDescriptor(window, "isSecureContext");
    mountChat();
    expect(screen.getByRole("button", { name: "Iniciar ditado" })).toBeTruthy();
    cleanup();
    Object.defineProperty(window, "isSecureContext", { value: false, configurable: true });
    try {
      mountChat();
      expect(screen.queryByRole("button", { name: "Iniciar ditado" })).toBeNull();
    } finally {
      if (secure) Object.defineProperty(window, "isSecureContext", secure);
      else delete (window as unknown as { isSecureContext?: boolean }).isSecureContext;
    }
  });

  it("does not show RAG or mention it in the request without a RAG server on the network", async () => {
    mountChat();
    expect(screen.queryByRole("button", { name: "RAG" })).toBeNull();
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "primeira" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    await waitFor(() => expect(streamScenario.lastBody?.messages).toBeTruthy());
    expect(streamScenario.lastBody && "rag" in streamScenario.lastBody).toBe(false);
  });

  it("does not show Think when the deployment disables thinking", () => {
    thinkingScenario.enabled = false;
    mountChat();
    expect(screen.queryByRole("button", { name: "Think" })).toBeNull();
  });

  it("continues the stream and shows the buffer when the route remounts", async () => {
    let release!: () => void;
    const pending = new Promise<void>((resolve) => { release = resolve; });
    streamScenario.release = () => release();

    function Toggle() {
      const [visible, setVisible] = useState(true);
      return (
        <>
          <button type="button" onClick={() => setVisible(false)}>sair da rota</button>
          <button type="button" onClick={() => setVisible(true)}>voltar ao chat</button>
          {visible && <Chat />}
        </>
      );
    }

    render(
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile="admin">
          <ThemeProvider>
            <ToastProvider>
              <ChatSessionProvider>
                <Toggle />
              </ChatSessionProvider>
            </ToastProvider>
          </ThemeProvider>
        </ProfileProvider>
      </LocaleProvider>,
    );

    const composer = await screen.findByRole("textbox");
    fireEvent.change(composer, { target: { value: "oi" } });
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));
    expect(await screen.findByText("primeiro")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "sair da rota" }));
    expect(screen.getByText("voltar ao chat")).toBeTruthy();
    expect(streamScenario.signalAborted).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "voltar ao chat" }));
    expect(screen.getByText("primeiro")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Parar" })).toBeTruthy();

    act(() => streamScenario.release?.());
    await pending;
    await waitFor(() => expect(screen.getByText(/segundo/)).toBeTruthy());
    expect(streamScenario.signalAborted).toBe(false);
  });

  it("renders the whole image and keeps the name as the caption", () => {
    const message: ChatMessage = {
      id: "user-1",
      role: "user",
      content: "Veja isto",
      attachments: [{ id: "image-1", name: "paisagem.png", kind: "image", data: "data:image/png;base64,abc" }],
      at: new Date().toISOString(),
    };

    render(
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile="admin">
          <Bubble message={message} streaming={false} />
        </ProfileProvider>
      </LocaleProvider>,
    );

    const image = screen.getByRole("img", { name: "Imagem enviada: paisagem.png" });
    expect(image.getAttribute("src")).toBe(message.attachments?.[0].data);
    expect(image.className).toContain("max-h");
    expect(screen.getByText("paisagem.png")).toBeTruthy();
  });

  it("transcribes dictation when the browser offers Web Speech", () => {
    class FakeRecognition {
      lang = "";
      continuous = false;
      interimResults = false;
      onresult: ((event: unknown) => void) | null = null;
      onend: (() => void) | null = null;
      start = vi.fn();
      stop = vi.fn(() => this.onend?.());

      emit(text: string) {
        this.onresult?.({ resultIndex: 0, results: [{ 0: { transcript: text }, isFinal: true }] });
      }
    }
    let instance: FakeRecognition | undefined;
    class RecognitionConstructor extends FakeRecognition {
      constructor() {
        super();
        instance = this;
      }
    }
    vi.stubGlobal("SpeechRecognition", RecognitionConstructor);

    mountChat();
    const microphone = screen.getByRole("button", { name: "Iniciar ditado" });
    fireEvent.click(microphone);
    expect(screen.getByRole("button", { name: "Parar ditado" })).toBeTruthy();
    expect(instance?.lang).toBe("pt-BR");
    act(() => instance?.emit("Olá, rede"));
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("Olá, rede");

    fireEvent.click(screen.getByRole("button", { name: "Parar ditado" }));
    expect(instance?.stop).toHaveBeenCalled();
  });

  it("hides the microphone when Web Speech does not exist", () => {
    resetBrowserSpeech();
    mountChat();
    expect(screen.queryByRole("button", { name: /ditado/i })).toBeNull();
  });

  it("uses a single picker without a restrictive accept and refuses non-image files above the context", async () => {
    const { container } = mountChat();
    const inputs = container.querySelectorAll<HTMLInputElement>('input[type="file"]');
    expect(inputs).toHaveLength(1);
    expect(inputs[0].getAttribute("accept")).toBeNull();

    const file = new File(["12345678901234567"], "dados.bin", { type: "application/octet-stream" });
    Object.defineProperty(file, "text", { value: async () => "12345678901234567" });
    fireEvent.change(inputs[0], { target: { files: [file] } });
    expect(await screen.findByText("Anexo grande demais")).toBeTruthy();
    expect(screen.getByText(/excede o contexto disponível/i)).toBeTruthy();
    expect(screen.queryByText("dados.bin")).toBeNull();
  });

  it("downsizes a large image to the budget before attaching it", async () => {
    const { container } = mountChat();
    const input = container.querySelector<HTMLInputElement>('input[type="file"]')!;
    const file = new File([new Uint8Array(1_172_228)], "paisagem.png", { type: "image/png" });

    fireEvent.change(input, { target: { files: [file] } });

    expect(await screen.findByText("paisagem.png")).toBeTruthy();
    expect(screen.queryByText("Anexo grande demais")).toBeNull();
    expect(imageResizeScenario.calls).toEqual([12]);
    expect(screen.getByText(/redimensionada automaticamente/i)).toBeTruthy();
  });

  it("sends the model the bytes of the downsized image", async () => {
    const { container } = mountChat();
    const input = container.querySelector<HTMLInputElement>('input[type="file"]')!;
    const file = new File([new Uint8Array(1_172_228)], "paisagem.png", { type: "image/png" });

    fireEvent.change(input, { target: { files: [file] } });
    await screen.findByText("paisagem.png");
    fireEvent.click(screen.getByRole("button", { name: "Enviar" }));

    await waitFor(() => {
      const messages = streamScenario.lastBody?.messages as Array<{ content?: unknown }> | undefined;
      expect(messages?.at(-1)?.content).toEqual([
        { type: "image_url", image_url: { url: "data:image/jpeg;base64,ZmFrZQ==" } },
      ]);
    });
  });

  it("shows a specific refusal when the context does not fit even the minimum thumbnail", async () => {
    imageResizeScenario.reject = true;
    const { container } = mountChat();
    const input = container.querySelector<HTMLInputElement>('input[type="file"]')!;
    const file = new File([new Uint8Array(1_172_228)], "paisagem.png", { type: "image/png" });

    fireEvent.change(input, { target: { files: [file] } });

    expect(await screen.findByText("Imagem recusada")).toBeTruthy();
    expect(screen.getByText(/a imagem ainda é grande demais para o contexto disponível/i)).toBeTruthy();
    expect(screen.queryByText("paisagem.png")).toBeNull();
  });

  it("accepts a file of any type when it fits in the context", async () => {
    const { container } = mountChat();
    const input = container.querySelector<HTMLInputElement>('input[type="file"]')!;
    const file = new File(["ok"], "dados.bin", { type: "application/octet-stream" });
    Object.defineProperty(file, "text", { value: async () => "ok" });
    fireEvent.change(input, { target: { files: [file] } });
    expect(await screen.findByText("dados.bin")).toBeTruthy();
  });
});
