// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";

vi.mock("../lib/queries", () => ({
  useCluster: () => ({ data: { peers: [{ node_id: "node-a", short_id: "node-a", node: { hostname: "ksia" } }] } }),
}));
vi.mock("./Logs", () => ({
  toFilters: (values: { node: string; user: string; apiKey: string; model: string; from: string; to: string }) => ({
    node_id: values.node || undefined,
    user: values.user || undefined,
    api_key: values.apiKey || undefined,
    model: values.model || undefined,
    from: values.from ? new Date(values.from).toISOString() : undefined,
    to: values.to ? new Date(values.to).toISOString() : undefined,
  }),
  StatsView: ({ filters }: { filters: Record<string, unknown> }) => (
    <div data-testid="stats-filters">{JSON.stringify(filters)}<span>stats-content</span></div>
  ),
}));

import { Statistics } from "./Statistics";

describe("Statistics", () => {
  afterEach(cleanup);

  it("renders the statistics screen outside Logs", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <Statistics />
      </LocaleProvider>,
    );

    expect(screen.getByText("Estatísticas")).toBeTruthy();
    expect(screen.getByText("stats-content")).toBeTruthy();
  });

  it("applies period, user, API key, model and node filters", async () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <Statistics />
      </LocaleProvider>,
    );

    fireEvent.change(screen.getByLabelText("Usuário"), { target: { value: "financeiro" } });
    fireEvent.change(screen.getByLabelText("Identidade da API key"), { target: { value: "key-123" } });
    fireEvent.change(screen.getByLabelText("Modelo"), { target: { value: "qwen" } });
    fireEvent.change(screen.getByLabelText("Filtrar por nó"), { target: { value: "node-a" } });
    fireEvent.change(screen.getByLabelText("De"), { target: { value: "2026-09-01T00:00" } });
    fireEvent.change(screen.getByLabelText("Até"), { target: { value: "2026-09-10T23:59" } });
    fireEvent.click(screen.getByRole("button", { name: "Aplicar filtros" }));

    await waitFor(() => {
      const filters = screen.getByTestId("stats-filters").textContent ?? "";
      expect(filters).toContain('"user":"financeiro"');
      expect(filters).toContain('"api_key":"key-123"');
      expect(filters).toContain('"model":"qwen"');
      expect(filters).toContain('"node_id":"node-a"');
      expect(filters).toContain("2026-09-01");
      expect(filters).toContain("2026-09-11");
    });
  });
});
