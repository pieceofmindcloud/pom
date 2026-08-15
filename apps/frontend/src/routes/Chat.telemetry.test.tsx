// @vitest-environment jsdom
/**
 * The telemetry table is the whole point of the diagnostic work: it has to show
 * prefill and decode as two independent rows. A single averaged row was what
 * hid which of the two phases was slow.
 */
import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { ExecutionTelemetry } from "./Chat";
import { LocaleProvider } from "../i18n/provider";
import type { PhaseTrace, RequestTrace } from "../lib/chatStream";

function phase(overrides: Partial<PhaseTrace>): PhaseTrace {
  return {
    processing_ms: 0, network_ms: 0, bytes_sent: 0, bytes_received: 0, calls: 0,
    client_encode_ms: 0, response_headers_ms: 0, response_body_ms: 0, client_decode_ms: 0,
    node_frame_decode_ms: 0, node_input_prepare_ms: 0, node_queue_ms: 0, node_lock_wait_ms: 0, node_setup_ms: 0,
    node_decode_ms: 0, node_backend_enqueue_ms: 0, node_backend_completion_ms: 0,
    node_residual_capture_d2h_sync_ms: 0, node_residual_ms: 0,
    node_logits_access_ms: 0, node_logits_clone_ms: 0, node_repetition_penalty_ms: 0,
    node_softmax_ms: 0, node_top_k_ms: 0, node_top_p_ms: 0, node_selection_ms: 0,
    node_token_text_ms: 0, node_sampling_ms: 0, node_encode_ms: 0,
    node_p50_ms: 0, node_p95_ms: 0,
    ...overrides,
  };
}

/** Shaped like a real two-peer run: a heavy prefill, then many cheap tokens. */
function mount(ui: React.ReactNode) {
  return render(<LocaleProvider initialLocale="pt-BR">{ui}</LocaleProvider>);
}

/** Shaped like a real two-peer run: a heavy prefill, then many cheap tokens. */
const trace: RequestTrace = {
  model: "qwen",
  input_tokens: 2_100,
  output_tokens: 34,
  prefill_ms: 900,
  total_bytes_sent: 600_000,
  total_bytes_received: 12_000,
  hops: [
    {
      node_id: "abcdef0123456789",
      start_layer: 0,
      end_layer: 15,
      prefill: phase({
        processing_ms: 880, network_ms: 40, calls: 5, bytes_sent: 4_096, bytes_received: 512_000,
        client_encode_ms: 3, client_decode_ms: 4, node_decode_ms: 810, node_residual_ms: 45,
        node_sampling_ms: 0, node_encode_ms: 12, node_p50_ms: 160, node_p95_ms: 210,
      }),
      decode: phase({
        processing_ms: 300, network_ms: 120, calls: 34, bytes_sent: 3_400, bytes_received: 278_000,
        client_encode_ms: 8, client_decode_ms: 9, node_decode_ms: 240, node_residual_ms: 30,
        node_sampling_ms: 0, node_encode_ms: 14, node_p50_ms: 7, node_p95_ms: 11,
      }),
    },
  ],
};

describe("ExecutionTelemetry", () => {
  afterEach(cleanup);

  /**
   * Four rows of numbers per hop, under every answer, is not what the operator
   * came to the chat for. Open on demand, closed on arrival.
   */
  it("arrives closed", () => {
    const { container } = mount(<ExecutionTelemetry trace={trace} />);
    const details = container.querySelector("details");
    expect(details).toBeTruthy();
    expect(details!.open).toBe(false);
  });

  it("gives each hop one prefill row and one decode row", () => {
    mount(<ExecutionTelemetry trace={trace} source="pipeline:abc→def" />);

    const rows = screen.getAllByRole("row");
    const prefill = rows.find((row) => within(row).queryByText("prefill"));
    const decode = rows.find((row) => within(row).queryByText("decode"));
    expect(prefill).toBeDefined();
    expect(decode).toBeDefined();

    // The two phases must never share a number.
    expect(within(prefill!).getByText("810 ms")).toBeTruthy();
    expect(within(prefill!).getByText("40 ms")).toBeTruthy();
    expect(within(decode!).getByText("240 ms")).toBeTruthy();
    expect(within(decode!).getByText("120 ms")).toBeTruthy();

    // Call counts stay per phase: 5 prompt chunks, 34 generated tokens.
    expect(within(prefill!).getByText("5")).toBeTruthy();
    expect(within(decode!).getByText("34")).toBeTruthy();
  });

  it("says the backend sync is off rather than reporting a bare zero", () => {
    mount(<ExecutionTelemetry trace={trace} />);
    expect(screen.getAllByText(/sync desligado/).length).toBe(2);
  });

  /**
   * A session saved before the phase split, or a peer still on the old binary,
   * hands us a hop with no `prefill` object. That crashed the whole screen.
   */
  it("renders a hop from before the phase split instead of crashing", () => {
    const legacy = {
      model: "qwen",
      input_tokens: 12,
      output_tokens: 4,
      prefill_ms: 90,
      total_bytes_sent: 10,
      total_bytes_received: 20,
      hops: [
        {
          node_id: "abcdef0123456789",
          start_layer: 0,
          end_layer: 15,
          prefill_ms: 80,
          decode_ms: 40,
          network_ms: 15,
          bytes_sent: 1_024,
          bytes_received: 2_048,
          calls: 6,
          prefill_calls: 2,
          decode_calls: 4,
          client_encode_ms: 3,
          response_headers_ms: 7,
          response_body_ms: 2,
          client_decode_ms: 1,
          node_frame_decode_ms: 1,
          node_input_prepare_ms: 2,
          node_queue_ms: 3,
          node_setup_ms: 4,
          node_decode_ms: 95,
          node_residual_ms: 6,
          node_sampling_ms: 7,
          node_encode_ms: 8,
          node_decode_p50_ms: 9,
          node_decode_p95_ms: 11,
        },
      ],
    } as unknown as RequestTrace;

    mount(<ExecutionTelemetry trace={legacy} />);

    // The numbers it does have still show up...
    expect(screen.getByText("95 ms")).toBeTruthy();
    expect(screen.getByText("L0-L15")).toBeTruthy();
    // ...and the row says why there is no prefill/decode split, rather than
    // printing one total twice under two labels.
    expect(screen.getByText(/telemetria antiga/i)).toBeTruthy();
  });
});
