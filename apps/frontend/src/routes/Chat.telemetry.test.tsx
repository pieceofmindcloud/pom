// @vitest-environment jsdom
/**
 * The telemetry table is the whole point of the diagnostic work: it has to show
 * prefill and decode as two independent rows. A single averaged row was what
 * hid which of the two phases was slow.
 */
import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Bubble, ChatError } from "./Chat";
import { ExecutionTelemetry, type ExecutionTelemetryEnvironment } from "../components/ExecutionTelemetry";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";
import type { ChatMessage } from "../lib/chatStore";
import type { GenerationTimings, PhaseTrace, RequestTrace } from "../lib/chatStream";

function phase(overrides: Partial<PhaseTrace>): PhaseTrace {
  return {
    processing_ms: 0, round_trip_ms: 0, network_ms: 0, bytes_sent: 0, bytes_received: 0, calls: 0,
    client_encode_ms: 0, response_headers_ms: 0, response_body_ms: 0, client_decode_ms: 0,
    node_frame_decode_ms: 0, node_input_prepare_ms: 0, node_queue_ms: 0, node_lock_wait_ms: 0, node_setup_ms: 0,
    node_decode_ms: 0, node_backend_enqueue_ms: 0, node_backend_completion_ms: 0,
    node_residual_capture_d2h_sync_ms: 0, node_residual_ms: 0,
    node_logits_access_ms: 0, node_logits_clone_ms: 0, node_repetition_penalty_ms: 0,
    node_softmax_ms: 0, node_top_k_ms: 0, node_top_p_ms: 0, node_selection_ms: 0,
    node_token_text_ms: 0, node_sampling_ms: 0, node_encode_ms: 0,
    node_p50_ms: 0, node_p95_ms: 0,
    tls_calls: 0, tls_handshake_ms: 0,
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
        processing_ms: 880, round_trip_ms: 920, network_ms: 40, calls: 5, bytes_sent: 4_096, bytes_received: 512_000,
        client_encode_ms: 3, client_decode_ms: 4, node_decode_ms: 810, node_residual_ms: 45,
        node_sampling_ms: 0, node_encode_ms: 12, node_p50_ms: 160, node_p95_ms: 210,
        selected_path: "dense/llama.cpp", executed_engine: "llama.cpp",
      }),
      decode: phase({
        processing_ms: 300, round_trip_ms: 420, network_ms: 120, calls: 34, bytes_sent: 3_400, bytes_received: 278_000,
        client_encode_ms: 8, client_decode_ms: 9, node_decode_ms: 240, node_residual_ms: 30,
        node_sampling_ms: 0, node_encode_ms: 14, node_p50_ms: 7, node_p95_ms: 11,
        selected_path: "moe/q4_0", executed_engine: "moe",
      }),
    },
  ],
};

const twoPeerTrace: RequestTrace = {
  ...trace,
  hops: [
    trace.hops[0],
    {
      node_id: "fedcba9876543210",
      start_layer: 16,
      end_layer: 31,
      prefill: phase({ round_trip_ms: 330, calls: 5 }),
      decode: phase({ round_trip_ms: 210, calls: 34 }),
    },
  ],
};

const contradictoryTrace: RequestTrace = {
  ...trace,
  hops: [{
    node_id: "abcdef0123456789",
    start_layer: 0,
    end_layer: 15,
    prefill: phase({
      processing_ms: 880,
      round_trip_ms: 920,
      calls: 5,
      selected_path: "dense/llama.cpp",
      executed_engine: "llama.cpp",
      engine_mismatch: false,
    }),
    decode: phase({
      processing_ms: 300,
      round_trip_ms: 420,
      calls: 34,
      selected_path: "dense/llama.cpp",
      executed_engine: "llama.cpp",
      engine_mismatch: false,
    }),
  }],
};

const environment: ExecutionTelemetryEnvironment = {
  snapshotCaptured: true,
  cluster: {
    peers: [
      {
        node_id: "abcdef0123456789",
        short_id: "abcdef0123",
        node: {
          hostname: "gpu-a",
          system: { cpu_count: 12, ram: { total_mb: 32768, available_mb: 16384 }, gpus: [{ name: "RTX A6000" }] },
          config: { n_gpu_layers: 12, n_ctx: 8192, n_batch: 512, fit_memory: true, fit_memory_margin_mb: 256, offload_kqv: true, max_sessions: 2 },
        },
      },
      {
        node_id: "fedcba9876543210",
        short_id: "fedcba9876",
        node: {
          hostname: "cpu-b",
          system: { cpu_count: 8, ram: { total_mb: 16384, available_mb: 4096 }, gpus: [] },
          config: { n_gpu_layers: 0, n_ctx: 4096, n_batch: 256, fit_memory: false, fit_memory_margin_mb: 256, offload_kqv: false, max_sessions: 1 },
        },
      },
    ],
  } as unknown as ExecutionTelemetryEnvironment["cluster"],
  network: { measurements: [{ from_node_id: "abcdef0123456789", to_node_id: "fedcba9876543210", rtt_ms: 12.5, transport: "libp2p" }] },
  runtimes: {
    "abcdef0123456789": { loaded: true, health: { profile_sync_enabled: true, selected_path: "moe/q4_0", moe_requested: true, moe_executor: "cpu-reference", moe_executor_reason: "dtype IQ3_S sem ponte CUDA; CPUReference selecionado", placement: { layers: [
      { layer: 0, device: "CUDA0", buffers: [{ buffer: "CUDA0", bytes: 10 }] },
      { layer: 1, device: "CUDA0", buffers: [{ buffer: "CPU", bytes: 10 }, { buffer: "CUDA0", bytes: 10 }] },
    ] } } as never },
    "fedcba9876543210": { loaded: true, health: { profile_sync_enabled: false, placement: { layers: [
      { layer: 16, device: "CPU", buffers: [{ buffer: "CPU", bytes: 10 }] },
    ] } } as never },
  },
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

  it("shows request-level TTFT, prefill speed, generation speed and total time", () => {
    const timings: GenerationTimings = {
      pipeline_first_token_ms: 2400,
      server_ttft_ms: 2400,
      client_ttft_ms: 2510,
      ttft_ms: 2400,
      prefill_ms: 2000,
      prefill_tokens: 100,
      prefill_tokens_per_sec: 50,
      generation_ms: 7600,
      generation_tokens: 20,
      generation_tokens_per_sec: 20 / 7.6,
      total_ms: 10000,
      total_tokens_per_sec: 2,
    };

    mount(<ExecutionTelemetry trace={trace} timings={timings} />);

    expect(screen.getByText("Primeiro token do pipeline")).toBeTruthy();
    expect(screen.getByText("TTFT do servidor")).toBeTruthy();
    expect(screen.getByText("Primeiro token no cliente")).toBeTruthy();
    expect(screen.getByText("50,0 tokens/s · 100 tokens do prompt")).toBeTruthy();
    expect(screen.getByText("2,6 tokens/s · 20 tokens")).toBeTruthy();
    expect(screen.getByText("10,0 s · 2,0 tokens/s")).toBeTruthy();
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

  it("shows the backend engine beside each phase", () => {
    mount(<ExecutionTelemetry trace={trace} />);

    expect(screen.getByText("motor")).toBeTruthy();
    expect(screen.getByText("llama.cpp")).toBeTruthy();
    expect(screen.getByText("motor MoE")).toBeTruthy();
  });

  it("separates the current health path from the path observed in this execution", () => {
    mount(<ExecutionTelemetry trace={contradictoryTrace} environment={environment} />);

    expect(screen.getByText("estado atual: caminho selecionado: moe/q4_0")).toBeTruthy();
    expect(screen.getByText("nesta execução: caminho selecionado: dense/llama.cpp")).toBeTruthy();
    expect(screen.getByText(/O estado atual diverge desta execução/)).toBeTruthy();
    expect(screen.getByText(/Os valores do ambiente são um snapshot atual de \/health/)).toBeTruthy();
    expect(screen.getAllByText("caminho selecionado: dense/llama.cpp").length).toBeGreaterThan(0);
  });

  it("shows unknown when a hop has no engine field", () => {
    mount(<ExecutionTelemetry trace={twoPeerTrace} />);

    expect(screen.getAllByText("desconhecido").length).toBe(2);
  });

  it("shows the observed aggregate RTT of the execution", () => {
    mount(<ExecutionTelemetry trace={trace} />);
    expect(screen.getByText(/RTT agregado da execução: 1340 ms/)).toBeTruthy();
  });

  it("shows outside-node accounting and first-token boundaries when available", () => {
    mount(<ExecutionTelemetry trace={{
      ...trace,
      trace_id: "trace-123",
      profile: "trace",
      request_state: "cold",
      spans: [{
        name: "route_resolution",
        span_id: "span-1",
        host: "gateway",
        started_at_ms: 1_700_000_000_000,
        start_offset_ms: 12,
        duration_ms: 8,
      }],
      outside_node_total_ms: 1_955,
      outside_node_accounted_ms: 8,
      outside_node_unaccounted_ms: 1_947,
      first_token: { engine_ttft_ms: 128, node_serialized_ms: 2, node_to_gateway_ms: 3 },
      client_ttft_ms: 2050,
      streaming: {
        time_to_first_chunk_ms: 2050,
        time_to_first_token_ms: 2050,
        stream_duration_ms: 1084,
        time_last_token_to_response_end_ms: 12,
        chunks: 119,
        bytes: 4096,
        tokens_per_chunk: 1,
      },
      decode_calls: 64,
      generated_tokens: 119,
      tokens_per_decode_call: 119 / 64,
      connection_new_calls: 1,
      connection_reused_calls: 65,
      connection_unknown_calls: 0,
    }} />);

    expect(screen.getByText("trace trace-123 · trace · cold")).toBeTruthy();
    expect(screen.getByText("1,96 s")).toBeTruthy();
    expect(screen.getByText(/primeiro token no motor: 128 ms/)).toBeTruthy();
    expect(screen.getByText(/nó até gateway: 3 ms/)).toBeTruthy();
    expect(screen.getByText(/último token até fim: 12 ms/)).toBeTruthy();
    expect(screen.getByText(/chamadas de decode: 64 · tokens: 119/)).toBeTruthy();
    expect(screen.getByText(/conexões: 1 novas · 65 reutilizadas/)).toBeTruthy();
  });

  it("does not infer backend sync state from a zero duration", () => {
    mount(<ExecutionTelemetry trace={trace} />);
    expect(screen.queryByText(/sync desligado/)).toBeNull();
    expect(screen.getAllByText(/conclusão 0 ms/).length).toBe(2);
  });

  it("shows directional RTT for every pipeline edge with its measurement scope", () => {
    mount(<ExecutionTelemetry trace={twoPeerTrace} environment={environment} />);
    expect(screen.getByText("RTT da aresta (origem -> destino)")).toBeTruthy();
    expect(screen.getByText("12,5 ms")).toBeTruthy();
    expect(screen.getByText(/peer de origem até o peer de destino/)).toBeTruthy();
  });

  it("shows capacity, requested config, effective placement and sync per machine", () => {
    mount(<ExecutionTelemetry trace={twoPeerTrace} environment={environment} />);
    expect(screen.getByText(/CPU: 12 núcleos/)).toBeTruthy();
    expect(screen.getByText(/RTX A6000/)).toBeTruthy();
    expect(screen.getByText(/Camadas GPU pedidas \(intenção\): 12/)).toBeTruthy();
    expect(screen.getByText(/Camadas com peso em GPU: 1/)).toBeTruthy();
    expect(screen.getByText(/Camadas mistas: 1/)).toBeTruthy();
    expect(screen.getByText(/Camadas com peso em CPU: 1/)).toBeTruthy();
    expect(screen.getByText(/Ajuste à memória: ligado/)).toBeTruthy();
    expect(screen.getByText(/Ajuste à memória: desligado/)).toBeTruthy();
    expect(screen.getByText(/Pedido de offload KV: ligado/)).toBeTruthy();
    expect(screen.getByText(/Pedido de offload KV: desligado/)).toBeTruthy();
    expect(screen.getByText("estado atual: caminho selecionado: moe/q4_0")).toBeTruthy();
    expect(screen.getByText("executor MoE: cpu-reference")).toBeTruthy();
    expect(screen.getByText(/motivo do executor MoE: dtype IQ3_S/)).toBeTruthy();
    expect(screen.getByText("estado atual: caminho selecionado: não informado")).toBeTruthy();
    expect(screen.getByText("POM_PROFILE_SYNC: ativo")).toBeTruthy();
    expect(screen.getByText("POM_PROFILE_SYNC: desligado")).toBeTruthy();
  });

  it("explains the single-peer case without inventing an edge RTT", () => {
    mount(<ExecutionTelemetry trace={trace} environment={{ ...environment, network: { measurements: [] } }} />);
    expect(screen.getByText(/não há arestas entre peers/)).toBeTruthy();
    expect(screen.queryByText("12,5 ms")).toBeNull();
  });

  it("renders missing environment measurements as missing, never as zero", () => {
    mount(<ExecutionTelemetry trace={twoPeerTrace} environment={{ snapshotCaptured: true, network: { measurements: [] }, runtimes: {} }} />);
    expect(screen.getByText("não medido")).toBeTruthy();
    expect(screen.getAllByText("não informado").length).toBeGreaterThan(0);
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
    expect(screen.getByText(/RTT agregado da execução: indisponível/)).toBeTruthy();
    // ...and the row says why there is no prefill/decode split, rather than
    // printing one total twice under two labels.
    expect(screen.getByText(/telemetria antiga/i)).toBeTruthy();
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
    timings: {
      ttft_ms: 100,
      prefill_ms: 80,
      prefill_tokens: 8,
      prefill_tokens_per_sec: 100,
      generation_ms: 500,
      generation_tokens: 2,
      generation_tokens_per_sec: 4,
      total_ms: 600,
      total_tokens_per_sec: 3.3,
    },
    trace,
  };

  function mountBubble(profile: "admin" | "node" | "infer") {
    return render(
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile={profile}>
          <Bubble message={tracedMessage} streaming={false} />
        </ProfileProvider>
      </LocaleProvider>,
    );
  }

  it("não mostra telemetria no chat para infer", () => {
    mountBubble("infer");

    expect(screen.getByText("resposta")).toBeTruthy();
    expect(screen.queryByText("pipeline:abc→def")).toBeNull();
    expect(screen.queryByText(/Primeiro token|RTT agregado da execução/)).toBeNull();
  });

  it.each(["admin", "node"] as const)("mantém telemetria no chat para o perfil %s", (profile) => {
    mountBubble(profile);

    expect(screen.getAllByText("pipeline:abc→def").length).toBeGreaterThan(0);
    expect(screen.getAllByText(/Primeiro token/).length).toBeGreaterThan(0);
  });
});

describe("ChatError", () => {
  afterEach(cleanup);

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

/**
 * O canal de cada salto, e o que o número do handshake significa.
 *
 * O handshake acontece uma vez por conexão e as conexões ficam no pool: quase
 * todo salto reporta zero. Se a tela mostrasse o número sem dizer isso, quem
 * olhasse um salto zerado concluiria que TLS é grátis e quem olhasse o salto
 * que abriu conexão concluiria que é caro por token. Por isso o rótulo é parte
 * do que se afirma aqui, não enfeite.
 */
function traceCom(prefill: Partial<PhaseTrace>): RequestTrace {
  return {
    model: "qwen",
    input_tokens: 10,
    output_tokens: 1,
    prefill_ms: 10,
    total_bytes_sent: 0,
    total_bytes_received: 0,
    hops: [
      {
        node_id: "no1",
        start_layer: 0,
        end_layer: 15,
        prefill: phase(prefill),
        decode: phase({ calls: 0 }),
      },
    ],
  };
}

describe("ExecutionTelemetry: o canal de cada salto", () => {
  afterEach(cleanup);

  const summaryTrace: RequestTrace = {
    ...traceCom({
      calls: 5_001,
      transport_summary: {
        fallback_calls: 1,
        transports: [
          {
            transport: "local",
            calls: 5_001,
            bytes_sent: 9_090_000,
            bytes_received: 960_000,
            rtt_min_ms: 10,
            rtt_median_ms: 10,
            rtt_p95_ms: 10,
            rtt_max_ms: 50,
            fallback_calls: 0,
          },
          {
            transport: "https",
            calls: 2,
            bytes_sent: 200,
            bytes_received: 400,
            rtt_min_ms: 30,
            rtt_median_ms: 40,
            rtt_p95_ms: 40,
            rtt_max_ms: 40,
            fallback_calls: 1,
          },
          {
            transport: "quic",
            calls: 3,
            bytes_sent: 300,
            bytes_received: 600,
            rtt_min_ms: 20,
            rtt_median_ms: 25,
            rtt_p95_ms: 30,
            rtt_max_ms: 35,
            fallback_calls: 0,
          },
        ],
        transport_switches: [{ from: "local", to: "https", calls: 1 }],
      },
    }),
  };

  it("diz que o salto foi cifrado e que o handshake é por conexão", () => {
    mount(<ExecutionTelemetry trace={traceCom({ calls: 1, tls_calls: 1, tls_handshake_ms: 4 })} />);
    expect(screen.getByText(/canal: TLS/)).toBeTruthy();
    expect(screen.getByText(/handshake 4 ms/)).toBeTruthy();
    expect(screen.getByText(/por conexão, não por token/)).toBeTruthy();
  });

  it("um salto em claro é visível como tal", () => {
    mount(<ExecutionTelemetry trace={traceCom({ calls: 2, tls_calls: 0 })} />);
    expect(screen.getByText(/canal: texto claro/)).toBeTruthy();
  });

  /** Uma virada de política pela metade tem que aparecer, não ser arredondada
   * para "cifrado". */
  it("mostra a fração quando só parte das chamadas foi cifrada", () => {
    mount(<ExecutionTelemetry trace={traceCom({ calls: 4, tls_calls: 3, tls_handshake_ms: 6 })} />);
    expect(screen.getByText(/TLS em 3\/4 chamadas/)).toBeTruthy();
    expect(screen.getByText(/as demais foram em claro/)).toBeTruthy();
  });

  it("mostra a distribuição de transporte e a troca sem listar cada chamada", () => {
    mount(<ExecutionTelemetry trace={summaryTrace} />);
    expect(screen.getByText(/local: 5001 chamadas/)).toBeTruthy();
    expect(screen.getByText(/RTT mínimo\/mediana\/p95\/máximo 10\/10\/10\/50 ms/)).toBeTruthy();
    expect(screen.getByText(/fallback total 1/)).toBeTruthy();
    expect(screen.getByText(/quic: 3 chamadas/)).toBeTruthy();
    expect(screen.getByText(/troca de transporte local -> https \(1\)/)).toBeTruthy();
  });
});

describe("ExecutionTelemetry: tensor parallelism", () => {
  afterEach(cleanup);

  it("shows TP telemetry without hiding pipeline rows", () => {
    const traceWithTp: RequestTrace = {
      ...trace,
      hops: [{
        ...trace.hops[0],
        decode: phase({
          calls: 14,
          node_decode_ms: 25_114,
          processing_ms: 25_114,
          tp: {
            enabled: true,
            rank: 0,
            size: 2,
            expected_reduces: 64,
            reduces: 64,
            wait_ms: 25_100,
            reduce_p50_ms: 1_789,
            reduce_p95_ms: 1_838,
            request_bytes: 4_000,
            response_bytes: 8_000,
            direct_calls: 14,
            tunnel_calls: 0,
            fallback_calls: 0,
            broadcast_calls: 14,
            broadcast_bytes: 2_000,
          },
        }),
      }],
    };
    const onAnalyze = vi.fn();

    mount(<ExecutionTelemetry trace={traceWithTp} onAnalyze={onAnalyze} />);

    expect(screen.getByText(/tensor parallelism/i)).toBeTruthy();
    expect(screen.getByText(/reduções \/ esperado/)).toBeTruthy();
    expect(screen.getAllByText(/25100 ms/).length).toBeGreaterThan(0);
    expect(screen.getByRole("button", { name: /analisar com ia/i })).toBeTruthy();
    expect(screen.getAllByText("prefill").length).toBeGreaterThan(0);

    screen.getByRole("button", { name: /analisar com ia/i }).click();
    expect(onAnalyze).toHaveBeenCalledWith(traceWithTp);
  });

  it("disables the AI action while another response is streaming", () => {
    const onAnalyze = vi.fn();

    mount(<ExecutionTelemetry trace={trace} onAnalyze={onAnalyze} analyzeDisabled />);

    const button = screen.getByRole("button", { name: /analisar com ia/i }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
  });
});
