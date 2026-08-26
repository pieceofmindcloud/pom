// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";

const mocks = vi.hoisted(() => ({ useRequestLog: vi.fn() }));

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
}));

import { LogsByToken } from "./LogsByToken";

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

describe("LogsByToken", () => {
  afterEach(() => {
    cleanup();
    mocks.useRequestLog.mockReset();
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
          <LogsByToken />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByLabelText("Identidade da API key")).toBeTruthy();
    expect(screen.getByLabelText("Texto no prompt")).toBeTruthy();
    expect(screen.getByText("incompleta")).toBeTruthy();
    expect(screen.getByText("key-finance")).toBeTruthy();
    expect(screen.getByText("Inspecionar requisição").closest("details")?.hasAttribute("open")).toBe(false);
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
        <LogsByToken />
      </LocaleProvider>,
    );

    fireEvent.click(screen.getByText("Inspecionar requisição"));

    expect(screen.getByText("relatorio trimestral confidencial")).toBeTruthy();
    expect(screen.getByText(/\"stream\": false/)).toBeTruthy();
    expect(screen.getByText("servidor reiniciou antes do fim")).toBeTruthy();
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
        <LogsByToken />
      </LocaleProvider>,
    );

    fireEvent.change(screen.getByLabelText("Identidade da API key"), {
      target: { value: "key-finance" },
    });
    fireEvent.change(screen.getByLabelText("Texto no prompt"), {
      target: { value: "trimestral" },
    });

    expect(mocks.useRequestLog).toHaveBeenLastCalledWith(
      expect.objectContaining({ api_key: "key-finance", prompt: "trimestral" }),
    );
  });
});
