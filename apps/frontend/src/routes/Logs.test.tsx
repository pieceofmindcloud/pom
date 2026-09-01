// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";

const mocks = vi.hoisted(() => ({ useRequestLog: vi.fn(), useRequestStats: vi.fn() }));

vi.stubGlobal("ResizeObserver", class {
  observe() {}
  disconnect() {}
  unobserve() {}
});

vi.mock("../lib/queries", () => ({
  useCluster: () => ({
    data: {
      peers: [
        {
          node_id: "node-a",
          short_id: "node-a",
          node: { hostname: "salem" },
        },
      ],
    },
  }),
  useRequestLog: mocks.useRequestLog,
  useRequestStats: mocks.useRequestStats,
}));

import { Logs } from "./Logs";

const result = {
  entries: [
    {
      id: "req-1",
      received_at: "2026-08-26T12:00:00Z",
      completed_at: "2026-08-26T12:00:01Z",
      route: "/v1/chat/completions",
      model: "qwen",
      api_key_id: "key-finance",
      api_key_label: "financeiro",
      api_key_node_id: "node-a",
      entry_node_id: "node-a",
      prompt: "relatorio trimestral confidencial",
      request_details: { model: "qwen", stream: false },
      result: null,
      telemetry: null,
      hops: [],
      source: "none",
      prompt_tokens: null,
      completion_tokens: null,
      duration_ms: null,
      status: "incomplete",
      error: "servidor reiniciou antes do fim",
    },
  ],
  total: 1,
  errors: 1,
  incomplete: 1,
  by_token: { financeiro: 1 },
  by_node: { "node-a": 1 },
  storage: "sqlite",
};

describe("Logs", () => {
  afterEach(() => {
    cleanup();
    mocks.useRequestLog.mockReset();
    mocks.useRequestStats.mockReset();
  });

  it("shows the audit identity, filters and incomplete state", () => {
    mocks.useRequestLog.mockReturnValue({
      data: result,
      isError: false,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    });

    render(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <Logs />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByLabelText("Identidade da API key")).toBeTruthy();
    expect(screen.getByLabelText("Texto no prompt")).toBeTruthy();
    expect(screen.getByText("incompleta")).toBeTruthy();
    expect(screen.getByText("key-finance")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Inspecionar requisição" })).toBeTruthy();
  });

  it("reveals the exact prompt and request details when inspected", () => {
    mocks.useRequestLog.mockReturnValue({
      data: result,
      isError: false,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    });

    render(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Inspecionar requisição" }));

    const requestJson = screen.getByRole("textbox", { name: "REQUEST" }) as HTMLTextAreaElement;
    expect(requestJson.value).toContain("relatorio trimestral confidencial");
    expect(requestJson.value).toContain('"stream": false');
    fireEvent.click(screen.getByRole("tab", { name: "RESPONSE" }));
    const responseJson = screen.getByRole("textbox", { name: "RESPONSE" }) as HTMLTextAreaElement;
    expect(responseJson.value).toContain("servidor reiniciou antes do fim");
  });

  it("passes key and prompt searches to the request query", () => {
    mocks.useRequestLog.mockReturnValue({
      data: result,
      isError: false,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    });

    render(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );

    fireEvent.change(screen.getByLabelText("Identidade da API key"), {
      target: { value: "key-finance" },
    });
    fireEvent.change(screen.getByLabelText("Texto no prompt"), {
      target: { value: "trimestral" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Buscar" }));

    expect(mocks.useRequestLog).toHaveBeenLastCalledWith(
      expect.objectContaining({ api_key: "key-finance", prompt: "trimestral" }),
    );
  });

  it("opens a three-tab object inspector for a request", () => {
    mocks.useRequestLog.mockReturnValue({
      data: result,
      isError: false,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    });

    render(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Inspecionar requisição" }));

    expect(screen.getByRole("tab", { name: "REQUEST" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "RESPONSE" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "TELEMETRIA" })).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "RESPONSE" }));
    expect(screen.getByRole("textbox", { name: "RESPONSE" })).toBeTruthy();
  });

  it("copies a labelled request bundle without credentials", async () => {
    mocks.useRequestLog.mockReturnValue({
      data: {
        ...result,
        entries: [{
          ...result.entries[0],
          request_details: {
            model: "qwen",
            headers: { Authorization: "Bearer super-secret", "x-api-key": "sk-live-secret" },
            header_list: [{ key: "X-Connection-Key", value: "connection-secret" }],
            api_key: "sk-body-secret",
          },
        }],
      },
      isError: false,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    });
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });

    render(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Inspecionar requisição" }));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Copiar dados da requisição" }));
    });

    const copied = writeText.mock.calls[0]?.[0] as string;
    expect(copied).toContain('"REQUEST"');
    expect(copied).toContain('"api_key_identity"');
    expect(copied).toContain("[REDACTED]");
    expect(copied).not.toContain("super-secret");
    expect(copied).not.toContain("sk-live-secret");
    expect(copied).not.toContain("connection-secret");
    expect(copied).not.toContain("sk-body-secret");
  });

  it("loads the server-aggregated statistics tab", () => {
    mocks.useRequestLog.mockReturnValue({
      data: result,
      isError: false,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    });
    mocks.useRequestStats.mockReturnValue({
      data: {
        total: 1,
        input_tokens: 10,
        output_tokens: 4,
        average_inference_ms: 100,
        average_ttft_ms: 25,
        by_token: { financeiro: 1 },
        by_node: { "node-a": 1 },
        by_day: { "2026-08-26": { requests: 1, input_tokens: 10, output_tokens: 4 } },
        by_model: { qwen: { requests: 1, average_tokens_per_sec: 4, average_inference_ms: 100 } },
        peer_consumption: { "node-a": { requests: 1, bytes_sent: 10, bytes_received: 20, network_ms: 3 } },
        terminal_states: { ok: 1 },
        cache: { eligible_requests: 1, hit_requests: 1, input_tokens: 10, cached_tokens: 4 },
        storage: "sqlite",
      },
      isError: false,
      isLoading: false,
      error: null,
      refetch: vi.fn(),
    });

    render(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );

    fireEvent.click(screen.getByRole("tab", { name: "ESTATÍSTICAS" }));

    expect(screen.getByText("Consumo por dia")).toBeTruthy();
    expect(screen.getByText("Requisições por API token")).toBeTruthy();
    expect(screen.getByText("Consumo por peer")).toBeTruthy();
    expect(mocks.useRequestStats).toHaveBeenCalledWith(expect.anything(), true);
  });

  it("requests the next page once when the infinite-scroll sentinel intersects", () => {
    const firstPage = { ...result, total: 2 };
    const requestedOffsets: number[] = [];
    let lastOffset: number | undefined;
    mocks.useRequestLog.mockImplementation((params: { offset?: number }) => {
      if (params.offset !== lastOffset) {
        requestedOffsets.push(params.offset ?? 0);
        lastOffset = params.offset;
      }
      return {
        data: params.offset ? { ...result, entries: [] } : firstPage,
        isError: false,
        isLoading: false,
        error: null,
        refetch: vi.fn(),
      };
    });
    mocks.useRequestStats.mockReturnValue({ data: null, isLoading: false, isError: false });
    let observerCallback: IntersectionObserverCallback | undefined;
    vi.stubGlobal("IntersectionObserver", class {
      constructor(callback: IntersectionObserverCallback) {
        observerCallback = callback;
      }
      observe() {}
      disconnect() {}
      unobserve() {}
    });

    render(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );

    expect(observerCallback).toBeDefined();
    act(() => observerCallback?.([{ isIntersecting: true } as IntersectionObserverEntry], {} as IntersectionObserver));
    act(() => observerCallback?.([{ isIntersecting: true } as IntersectionObserverEntry], {} as IntersectionObserver));

    expect(requestedOffsets.filter((offset) => offset === 50)).toHaveLength(1);
    expect(screen.getByTestId("logs-load-more").getAttribute("data-page")).toBe("1");
  });
});
