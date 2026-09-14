// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useEffect, useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Chat } from "./Chat";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";
import { ThemeProvider } from "../lib/theme";
import { ToastProvider } from "../lib/toast";
import { ChatSessionProvider } from "../lib/chatStore";

const trace = {
  model: "qwen",
  input_tokens: 10,
  output_tokens: 3,
  prefill_ms: 80,
  total_bytes_sent: 100,
  total_bytes_received: 100,
  hops: [
    {
      node_id: "abcdef0123456789",
      start_layer: 0,
      end_layer: 15,
    },
    {
      node_id: "fedcba9876543210",
      start_layer: 16,
      end_layer: 31,
    },
  ],
};

const timings = {
  pipeline_first_token_ms: 80,
  ttft_ms: 80,
  prefill_ms: 50,
  prefill_tokens: 10,
  prefill_tokens_per_sec: 200,
  generation_ms: 150,
  generation_tokens: 3,
  generation_tokens_per_sec: 20,
  total_ms: 230,
  total_tokens_per_sec: 13,
};

let streamIndex = 0;

vi.mock("../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/api")>();
  return {
    ...actual,
    // eslint-disable-next-line require-yield
    streamSse: async function* () {
      const index = ++streamIndex;
      yield JSON.stringify({ choices: [{ delta: { content: index === 1 ? "oi" : `resposta ${index}` } }] });
      yield JSON.stringify({
        choices: [{ delta: {} }],
        x_pom: { trace, timings, source: `gateway-${index}` },
      });
      yield "[DONE]";
    },
  };
});

// Simulates react-query resolving *after* the stream has already finished and
// the assistant bubble has already rendered once without network data.
vi.mock("../lib/queries", () => ({
  useNode: () => ({
    data: { active_model: "qwen", config: { n_ctx: 4, max_new_tokens: 64 } },
    isLoading: false,
    isError: false,
  }),
  useCluster: () => ({ data: null }),
  useChatCapabilities: () => ({
    data: { vision: "supported", coverage: "complete", thinking: "unsupported" },
  }),
  useServableModels: () => ({ data: [{ id: "qwen", context_length: 4 }] }),
  useInferenceHeads: () => ({ data: undefined, isLoading: false, isError: false }),
  useChatTelemetryNetwork: (_nodeIds: string[], enabled: boolean, _messageId?: string) => {
    const [data, setData] = useState<unknown>(undefined);
    useEffect(() => {
      if (!enabled) return;
      const rtt = streamIndex === 1 ? 12.5 : 25;
      const timer = setTimeout(() => {
        setData({ measurements: [{ from_node_id: "abcdef0123456789", to_node_id: "fedcba9876543210", rtt_ms: rtt, transport: "libp2p" }] });
      }, 30);
      return () => clearTimeout(timer);
    }, [enabled, _messageId]);
    return { data, isLoading: !data, isError: false };
  },
  usePeerRuntimes: () => [],
}));

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

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  streamIndex = 0;
});

describe("telemetria do rodapé atualiza sem F5", () => {
  it("mostra o RTT de rede assim que a consulta resolve, sem remontar a tela", async () => {
    mountChat();

    const composer = await screen.findByRole("textbox");
    act(() => {
      fireEvent.change(composer, { target: { value: "oi" } });
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: /enviar/i }));
    });

    await screen.findByText("oi", { selector: "p" });

    // The final capture must mount the footer even if the transcript patch and
    // lastStreamResult are observed in separate React commits.
    await waitFor(
      () => expect(screen.getByText(/Telemetria da execução/)).toBeTruthy(),
      { timeout: 2000 },
    );

    // The network RTT query resolves ~30ms after the stream ends. Without a
    // remount, this text must appear once the query settles.
    await waitFor(
      () => expect(screen.getByText(/12,5|12\.5/)).toBeTruthy(),
      { timeout: 2000 },
    );
  });

  it("mantém resumo, card e ambiente de cada resposta anterior", async () => {
    mountChat();

    const composer = await screen.findByRole("textbox");
    const send = async (content: string, reply: string) => {
      act(() => {
        fireEvent.change(composer, { target: { value: content } });
        fireEvent.click(screen.getByRole("button", { name: /enviar/i }));
      });
      await screen.findByText(reply, { selector: "p" });
    };

    await send("primeira", "oi");
    await waitFor(() => expect(screen.getByText("12,5 ms")).toBeTruthy(), { timeout: 2000 });
    await send("segunda", "resposta 2");

    await waitFor(() => {
      expect(screen.getAllByText(/Telemetria da execução/)).toHaveLength(2);
      expect(screen.getAllByText("Geração")).toHaveLength(2);
      expect(screen.getByText("gateway-1")).toBeTruthy();
      expect(screen.getByText("gateway-2")).toBeTruthy();
      expect(screen.getByText("12,5 ms")).toBeTruthy();
      expect(screen.getAllByText("25 ms").length).toBeGreaterThan(0);
    }, { timeout: 2000 });
  });
});
