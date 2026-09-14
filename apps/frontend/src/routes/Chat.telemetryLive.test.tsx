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

vi.mock("../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/api")>();
  return {
    ...actual,
    // eslint-disable-next-line require-yield
    streamSse: async function* () {
      yield JSON.stringify({ choices: [{ delta: { content: "oi" } }] });
      yield JSON.stringify({
        choices: [{ delta: {} }],
        x_pom: { trace, source: "gateway" },
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
  useChatTelemetryNetwork: (_nodeIds: string[], enabled: boolean) => {
    const [data, setData] = useState<unknown>(undefined);
    useEffect(() => {
      if (!enabled) return;
      const timer = setTimeout(() => {
        setData({ measurements: [{ from_node_id: "abcdef0123456789", to_node_id: "fedcba9876543210", rtt_ms: 12.5, transport: "libp2p" }] });
      }, 30);
      return () => clearTimeout(timer);
    }, [enabled]);
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
});
