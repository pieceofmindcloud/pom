// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
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
    window.localStorage.clear();
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
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
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
    window.localStorage.setItem("pom.logs.inspector.format", "raw");
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

    fireEvent.click(screen.getByRole("tab", { name: "REQUEST" }));
    const requestJson = screen.getByRole("textbox", { name: "REQUEST" });
    expect(requestJson.textContent).toContain("relatorio trimestral confidencial");
    expect(requestJson.textContent).toContain('"stream": false');
    expect(requestJson.querySelector("code.hljs.language-json")).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Resposta" }));
    const responseJson = screen.getByRole("textbox", { name: "Resposta" });
    expect(responseJson.textContent).toContain("servidor reiniciou antes do fim");
  });

  it("keeps every field returned by the audit record in its inspector section", () => {
    window.localStorage.setItem("pom.logs.inspector.format", "raw");
    mocks.useRequestLog.mockReturnValue({
      data: {
        ...result,
        entries: [{
          ...result.entries[0],
          completed_at: "2026-08-26T12:00:01Z",
          result: "resposta gravada",
          source: "pipeline:node-a→node-b",
          hops: ["node-a", "node-b"],
          prompt_tokens: 10,
          completion_tokens: 4,
          duration_ms: 1000,
          timings: {
            ttft_ms: 25,
            prefill_ms: 20,
            prefill_tokens: 10,
            prefill_tokens_per_sec: 500,
            generation_ms: 75,
            generation_tokens: 4,
            generation_tokens_per_sec: 53.3,
            total_ms: 100,
            total_tokens_per_sec: 40,
          },
          telemetry: { model: "qwen", input_tokens: 10, output_tokens: 4 },
          status: "ok",
          error: null,
        }],
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

    fireEvent.click(screen.getByRole("button", { name: "Inspecionar requisição" }));
    fireEvent.click(screen.getByRole("tab", { name: "REQUEST" }));
    const requestJson = screen.getByRole("textbox", { name: "REQUEST" });
    expect(requestJson.textContent).toContain('"id": "req-1"');
    expect(requestJson.textContent).toContain('"received_at": "2026-08-26T12:00:00Z"');
    expect(requestJson.textContent).toContain('"route": "/v1/chat/completions"');
    expect(requestJson.textContent).toContain('"model": "qwen"');
    expect(requestJson.textContent).toContain('"api_key_identity"');
    expect(requestJson.textContent).toContain('"node_id": "node-a"');
    expect(requestJson.textContent).toContain('"entry_node_id": "node-a"');
    expect(requestJson.textContent).toContain('"prompt": "relatorio trimestral confidencial"');
    expect(requestJson.textContent).toContain('"request_details"');

    fireEvent.click(screen.getByRole("tab", { name: "Resposta" }));
    const responseJson = screen.getByRole("textbox", { name: "Resposta" });
    expect(responseJson.textContent).toContain('"completed_at": "2026-08-26T12:00:01Z"');
    expect(responseJson.textContent).toContain('"status": "ok"');
    expect(responseJson.textContent).toContain('"result": "resposta gravada"');
    expect(responseJson.textContent).toContain('"error": null');
    expect(responseJson.textContent).toContain('"source": "pipeline:node-a→node-b"');
    expect(responseJson.textContent).toContain('"hops"');
    expect(responseJson.textContent).toContain('"prompt_tokens": 10');
    expect(responseJson.textContent).toContain('"completion_tokens": 4');
    expect(responseJson.textContent).toContain('"duration_ms": 1000');

    fireEvent.click(screen.getByRole("tab", { name: "TELEMETRIA" }));
    const telemetryJson = screen.getByRole("textbox", { name: "TELEMETRIA" });
    expect(telemetryJson.textContent).toContain('"timings"');
    expect(telemetryJson.textContent).toContain('"telemetry"');
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

  it("opens a four-tab object inspector for a request", () => {
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
    expect(screen.getByRole("tab", { name: "Resposta" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "TELEMETRIA" })).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Resposta" }));
    expect(screen.getByLabelText("Resposta")).toBeTruthy();
  });

  it("opens the inspector from any non-interactive part of a row and highlights it", () => {
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

    const row = screen.getByTestId("logs-row-req-1");
    fireEvent.click(within(row).getByText("qwen"));

    expect(screen.getByRole("heading", { name: "Inspetor de requisição" })).toBeTruthy();
    expect(row.getAttribute("aria-selected")).toBe("true");
    expect(row.className).toContain("bg-iris/20");
    expect(row.className).toContain("ring-iris/60");
  });

  it("does not move the error badge when loading more appears and disappears", () => {
    let fetching = false;
    mocks.useRequestLog.mockImplementation(() => ({
      data: result,
      isError: false,
      isLoading: false,
      isFetching: fetching,
      error: null,
      refetch: vi.fn(),
    }));

    const { rerender } = render(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );

    const badge = screen.getByTestId("logs-errors-badge");
    const status = badge.parentElement!;
    const badgePosition = [...status.children].indexOf(badge);
    expect(status.className).toContain("min-w-");

    fetching = true;
    rerender(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );
    expect([...badge.parentElement!.children].indexOf(badge)).toBe(badgePosition);
    expect(screen.getByText("Carregando mais")).toBeTruthy();

    fetching = false;
    rerender(
      <LocaleProvider initialLocale="pt-BR">
        <Logs />
      </LocaleProvider>,
    );
    expect([...badge.parentElement!.children].indexOf(badge)).toBe(badgePosition);
    expect(screen.queryByText("Carregando mais")).toBeNull();
  });

  it("copies a labelled request bundle without credentials", async () => {
    window.localStorage.setItem("pom.logs.inspector.format", "raw");
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
    fireEvent.click(screen.getByRole("tab", { name: "REQUEST" }));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Copiar conteúdo da aba REQUEST" }));
    });

    const copied = writeText.mock.calls[0]?.[0] as string;
    expect(copied).toContain('"id": "req-1"');
    expect(copied).toContain('"api_key_identity"');
    expect(copied).toContain("[REDACTED]");
    expect(copied).not.toContain("super-secret");
    expect(copied).not.toContain("sk-live-secret");
    expect(copied).not.toContain("connection-secret");
    expect(copied).not.toContain("sk-body-secret");
  });

  it("does not render the removed copy warning", () => {
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

    expect(screen.queryByText(/Credenciais viram \[REDACTED\]/)).toBeNull();
    expect(screen.queryByText(/JSON pode ser selecionado/)).toBeNull();
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
