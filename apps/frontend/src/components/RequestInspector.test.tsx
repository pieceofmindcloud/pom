// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { RequestTrace, PhaseTrace } from "../lib/chatStream";
import type { RequestLogEntry } from "../lib/types";
import { RequestInspector, redactSensitive } from "./RequestInspector";

function phase(overrides: Partial<PhaseTrace> = {}): PhaseTrace {
  return {
    processing_ms: 40,
    round_trip_ms: 50,
    network_ms: 10,
    bytes_sent: 100,
    bytes_received: 200,
    calls: 1,
    client_encode_ms: 1,
    response_headers_ms: 2,
    response_body_ms: 3,
    client_decode_ms: 4,
    tls_calls: 0,
    tls_handshake_ms: 0,
    node_frame_decode_ms: 5,
    node_input_prepare_ms: 6,
    node_queue_ms: 7,
    node_lock_wait_ms: 8,
    node_setup_ms: 9,
    node_decode_ms: 10,
    node_backend_enqueue_ms: 11,
    node_backend_completion_ms: 12,
    node_residual_capture_d2h_sync_ms: 13,
    node_residual_ms: 14,
    node_logits_access_ms: 15,
    node_logits_clone_ms: 16,
    node_repetition_penalty_ms: 17,
    node_softmax_ms: 18,
    node_top_k_ms: 19,
    node_top_p_ms: 20,
    node_selection_ms: 21,
    node_token_text_ms: 22,
    node_sampling_ms: 23,
    node_encode_ms: 24,
    node_p50_ms: 25,
    node_p95_ms: 26,
    ...overrides,
  };
}

const trace: RequestTrace = {
  model: "qwen",
  input_tokens: 4,
  output_tokens: 2,
  prefill_ms: 40,
  total_bytes_sent: 200,
  total_bytes_received: 400,
  hops: [{
    node_id: "node-a",
    start_layer: 0,
    end_layer: 3,
    prefill: phase({ calls: 2 }),
    decode: phase({ calls: 3 }),
  }],
};

const entry: RequestLogEntry = {
  id: "req-1",
  received_at: "2026-08-26T12:00:00Z",
  completed_at: "2026-08-26T12:00:01Z",
  route: "/v1/chat/completions",
  model: "qwen",
  api_key_id: "key-1",
  api_key_label: "operator",
  api_key_node_id: null,
  entry_node_id: "node-a",
  prompt: "prompt de teste",
  request_details: { model: "qwen", stream: false },
  result: "resposta",
  telemetry: trace as unknown as Record<string, unknown>,
  hops: ["node-a"],
  source: "pipeline:node-a",
  prompt_tokens: 4,
  completion_tokens: 2,
  duration_ms: 100,
  timings: null,
  status: "ok",
  error: null,
};

function mountInspector() {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <RequestInspector entry={entry} onClose={() => undefined} />
    </LocaleProvider>,
  );
}

describe("redactSensitive", () => {
  it("redacts uppercase and hyphenated credential keys", () => {
    expect(redactSensitive({ Authorization: "Bearer segredo", "X-API-Key": "sk-segredo" })).toEqual({
      Authorization: "[REDACTED]",
      "X-API-Key": "[REDACTED]",
    });
  });

  it("redacts sensitive values through nested objects", () => {
    expect(redactSensitive({ request: { client_secret: "segredo", body: { password: "senha" } } })).toEqual({
      request: { client_secret: "[REDACTED]", body: { password: "[REDACTED]" } },
    });
  });

  it("redacts named sensitive headers inside lists", () => {
    expect(redactSensitive({ headers: [{ name: "Authorization", value: "Bearer segredo" }, { name: "Content-Type", value: "application/json" }] })).toEqual({
      headers: [{ name: "Authorization", value: "[REDACTED]" }, { name: "Content-Type", value: "application/json" }],
    });
  });

  it("redacts secret-shaped keys in api key records", () => {
    expect(redactSensitive({ api_keys: [{ key: "sk-segredo", label: "opencode" }] })).toEqual({
      api_keys: [{ key: "[REDACTED]", label: "opencode" }],
    });
  });

  it("keeps innocent key fields intact", () => {
    expect(redactSensitive({ metadata: { key: "public-key", label: "document" }, api_keys: [{ key: "not-a-secret", label: "opencode" }, { key: "sketch", label: "opencode" }] })).toEqual({
      metadata: { key: "public-key", label: "document" },
      api_keys: [{ key: "not-a-secret", label: "opencode" }, { key: "sketch", label: "opencode" }],
    });
  });
});

describe("RequestInspector", () => {
  afterEach(() => {
    cleanup();
    window.localStorage.clear();
  });

  it("shows the new prompt and previous context as separate markdown editors", () => {
    const conversationalEntry = {
      ...entry,
      request_details: {
        model: "qwen",
        messages: [
          { role: "system", content: "Você é um assistente." },
          { role: "user", content: "A pergunta anterior" },
          { role: "assistant", content: "A resposta anterior" },
          { role: "user", content: "A pergunta nova" },
        ],
      },
    };

    render(
      <LocaleProvider initialLocale="pt-BR">
        <RequestInspector entry={conversationalEntry} onClose={() => undefined} />
      </LocaleProvider>,
    );

    fireEvent.click(screen.getByRole("tab", { name: "ENVIADO" }));

    const prompt = screen.getByRole("textbox", { name: "Prompt novo" }) as HTMLTextAreaElement;
    const context = screen.getByRole("textbox", { name: "Contexto anterior" }) as HTMLTextAreaElement;
    expect(prompt.value).toContain("A pergunta nova");
    expect(context.value).toContain("### system");
    expect(context.value).toContain("A resposta anterior");
    expect(context.value).not.toContain("A pergunta nova");
    expect(prompt.value).not.toContain('"messages"');
  });

  it("keeps the response in the same markdown editor", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <RequestInspector entry={entry} onClose={() => undefined} />
      </LocaleProvider>,
    );

    fireEvent.click(screen.getByRole("tab", { name: "Resposta" }));

    const response = screen.getByRole("textbox", { name: "Resposta" }) as HTMLTextAreaElement;
    expect(response.value).toBe("resposta");
    expect(response.value).not.toContain('"result"');
  });

  it("persists and respects the raw versus formatted preference", () => {
    window.localStorage.setItem("pom.logs.inspector.format", "raw");
    const view = render(
      <LocaleProvider initialLocale="pt-BR">
        <RequestInspector entry={entry} onClose={() => undefined} />
      </LocaleProvider>,
    );

    const preference = screen.getByLabelText("Formato do inspetor") as HTMLSelectElement;
    expect(preference.value).toBe("raw");
    fireEvent.click(screen.getByRole("tab", { name: "REQUEST" }));
    expect(screen.getByRole("textbox", { name: "REQUEST" }).textContent).toContain('"request_details"');

    fireEvent.change(preference, { target: { value: "formatted" } });
    expect(window.localStorage.getItem("pom.logs.inspector.format")).toBe("formatted");
    fireEvent.click(screen.getByRole("tab", { name: "Resposta" }));
    expect((screen.getByRole("textbox", { name: "Resposta" }) as HTMLTextAreaElement).value).toBe("resposta");

    view.unmount();
    render(
      <LocaleProvider initialLocale="pt-BR">
        <RequestInspector entry={entry} onClose={() => undefined} />
      </LocaleProvider>,
    );
    expect((screen.getByLabelText("Formato do inspetor") as HTMLSelectElement).value).toBe("formatted");
  });

  it("shows the request path separately from the shared telemetry card", () => {
    mountInspector();

    fireEvent.click(screen.getByRole("tab", { name: "TELEMETRIA" }));

    expect(screen.getByText("Caminho da requisição")).toBeTruthy();
    expect(screen.getByText("pipeline:node-a")).toBeTruthy();
    const card = screen.getByText(/Telemetria da execução/).closest("details");
    expect(card).toBeTruthy();
    fireEvent.click(card!.querySelector("summary")!);
    expect(within(card!).getByText("prefill")).toBeTruthy();
    expect(within(card!).getByText("Leitura da execução")).toBeTruthy();
  });
});
