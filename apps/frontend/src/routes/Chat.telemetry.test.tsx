// @vitest-environment jsdom
/**
 * The open-source Chat shows one compact telemetry line per answer. The
 * detailed view belongs to a plugin claiming the `chat.telemetry` slot; these
 * tests pin the line, the profile gate and the props the slot receives.
 */
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Bubble, ChatError } from "./Chat";
import { TelemetryLine } from "../components/TelemetryLine";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";
import type { ChatMessage } from "../lib/chatStore";
import type { ChatTelemetrySlotProps } from "../lib/chatTelemetry";
import type { GenerationTimings, RequestTrace } from "../lib/chatStream";
import type { ResolvedPluginSlot } from "../lib/pluginUi";

const slotProps: ChatTelemetrySlotProps[] = [];

vi.mock("../components/PluginSlot", () => ({
  usePluginSlot: () => null,
  PluginSlot: ({ props }: { props: ChatTelemetrySlotProps }) => {
    slotProps.push(props);
    return <button type="button">telemetria avançada</button>;
  },
}));

function mount(ui: React.ReactNode) {
  return render(<LocaleProvider initialLocale="pt-BR">{ui}</LocaleProvider>);
}

const trace: RequestTrace = {
  model: "qwen",
  input_tokens: 2_100,
  output_tokens: 34,
  prefill_ms: 900,
  total_bytes_sent: 600_000,
  total_bytes_received: 12_000,
  hops: [],
};

const timings: GenerationTimings = {
  ttft_ms: 950,
  pipeline_first_token_ms: 940,
  prefill_ms: 900,
  prefill_tokens: 2_100,
  prefill_tokens_per_sec: 2_333.3,
  generation_ms: 1_200,
  generation_tokens: 34,
  generation_tokens_per_sec: 28.3,
  total_ms: 2_150,
  total_tokens_per_sec: 15.8,
};

const plugin: ResolvedPluginSlot = {
  code: "logs",
  slot: "chat.telemetry",
  screen: { module: "ui/screens.js", export: "chatTelemetry" },
  version: "1.0.0",
};

afterEach(() => {
  cleanup();
  slotProps.length = 0;
});

describe("TelemetryLine", () => {
  it("shows generation speed, TTFT, total time and prompt processing on one line", () => {
    mount(<TelemetryLine trace={trace} timings={timings} source="pipeline:abc→def" />);

    const line = screen.getByTestId("telemetry-line");
    expect(line.textContent).toContain("28,3 tok/s");
    expect(line.textContent).toContain("34 tokens");
    expect(line.textContent).toContain("TTFT 950 ms");
    expect(line.textContent).toContain("total");
    expect(line.textContent).toContain("prompt 2100 tok");
    expect(screen.getByText("pipeline:abc→def")).toBeTruthy();
  });

  it("falls back to the trace when the answer has no timings", () => {
    mount(<TelemetryLine trace={{ ...trace, server_ttft_ms: 120 }} />);

    const line = screen.getByTestId("telemetry-line");
    expect(line.textContent).toContain("34 tokens");
    expect(line.textContent).toContain("TTFT 120 ms");
    // 2100 tokens in 900 ms.
    expect(line.textContent).toContain("2.333,3 tok/s");
  });
});

describe("telemetria no chat por perfil", () => {
  const tracedMessage: ChatMessage = {
    id: "assistant-1",
    role: "assistant",
    content: "resposta",
    at: "2026-09-02T00:00:00Z",
    source: "pipeline:abc→def",
    tokensPerSecond: 4,
    timings,
    trace,
  };

  function mountBubble(profile: "admin" | "node" | "infer", extra: Partial<Parameters<typeof Bubble>[0]> = {}) {
    return mount(
      <ProfileProvider initialProfile={profile}>
        <Bubble message={tracedMessage} streaming={false} {...extra} />
      </ProfileProvider>,
    );
  }

  it("não mostra telemetria no chat para infer", () => {
    mountBubble("infer", { telemetryPlugin: plugin });

    expect(screen.getByText("resposta")).toBeTruthy();
    expect(screen.queryByTestId("telemetry-line")).toBeNull();
    expect(screen.queryByText("pipeline:abc→def")).toBeNull();
    expect(screen.queryByText("telemetria avançada")).toBeNull();
  });

  it.each(["admin", "node"] as const)("mostra a linha básica para o perfil %s sem plugin", (profile) => {
    mountBubble(profile);

    expect(screen.getByTestId("telemetry-line")).toBeTruthy();
    expect(screen.getByText("pipeline:abc→def")).toBeTruthy();
    expect(screen.queryByText("telemetria avançada")).toBeNull();
  });

  it("monta o slot do plugin no fim da linha com o trace, o ambiente e a ação de perguntar", () => {
    const onAsk = vi.fn();
    const environment = { snapshotCaptured: true, runtimes: {} };
    mountBubble("admin", { telemetryPlugin: plugin, onAsk, telemetryEnvironment: environment });

    const line = screen.getByTestId("telemetry-line");
    expect(line.contains(screen.getByText("telemetria avançada"))).toBe(true);
    const props = slotProps.at(-1)!;
    expect(props.messageId).toBe("assistant-1");
    expect(props.trace).toBe(trace);
    expect(props.timings).toBe(timings);
    expect(props.source).toBe("pipeline:abc→def");
    expect(props.environment).toBe(environment);
    expect(props.onAsk).toBe(onAsk);
    expect(props.askDisabled).toBe(false);
  });

  it("não monta o slot enquanto a resposta ainda está chegando", () => {
    mountBubble("admin", { telemetryPlugin: plugin, streaming: true });

    expect(screen.queryByText("telemetria avançada")).toBeNull();
  });

  it("renderiza o trace capturado enquanto a mensagem ainda não recebeu o campo trace", () => {
    mount(
      <ProfileProvider initialProfile="admin">
        <Bubble
          message={{ ...tracedMessage, source: undefined, timings: undefined, trace: undefined }}
          telemetryTrace={trace}
          telemetryPlugin={plugin}
          streaming={false}
        />
      </ProfileProvider>,
    );

    expect(screen.getByTestId("telemetry-line")).toBeTruthy();
    expect(slotProps.at(-1)?.trace).toBe(trace);
  });
});

describe("ChatError", () => {
  it("shows a streamed server message in a readable, copyable block", () => {
    const message = "peer error: ".concat("x".repeat(180));
    mount(<ChatError message={message} />);

    expect(screen.getByText("Erro do servidor")).toBeTruthy();
    const errorText = screen.getByText(message);
    expect(errorText.className).toContain("whitespace-pre-wrap");
    expect(errorText.className).toContain("select-text");
    expect(screen.getByRole("button", { name: "Copiar erro" })).toBeTruthy();
  });
});
