// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { Transfers } from "./Transfers";

const queryResult = (data: unknown) => ({
  data,
  isError: false,
  isLoading: false,
  error: null,
  refetch: vi.fn(),
});

const { transfersState } = vi.hoisted(() => ({
  transfersState: { value: { jobs: [] as unknown[] } },
}));

vi.mock("../components/ModelDownloadsPanel", () => ({
  ModelDownloadsPanel: () => <div data-testid="model-downloads-panel">model downloads</div>,
}));
vi.mock("../lib/api", () => ({ api: { post: vi.fn() } }));
vi.mock("../lib/queries", () => ({
  useModelDownloads: vi.fn(() => queryResult([])),
  useOffers: vi.fn(() => queryResult({ offers: [] })),
  useTransfers: vi.fn(() => queryResult(transfersState.value)),
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), error: vi.fn(), info: vi.fn(), fromError: vi.fn() }),
}));

afterEach(cleanup);

function renderScreen() {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <Transfers />
      </MemoryRouter>
    </LocaleProvider>,
  );
}

describe("Transfers", () => {
  afterEach(() => {
    transfersState.value = { jobs: [] };
  });

  it("shows remote model downloads in the in-progress screen", () => {
    renderScreen();
    expect(screen.getByTestId("model-downloads-panel")).toBeTruthy();
  });

  it("shows an active transfer as a full-width table row with its progress", () => {
    transfersState.value = {
      jobs: [
        {
          id: "job-1",
          state: "running",
          direction: "sending",
          name: "abc123.gguf",
          peer_host: "atena",
          bytes_done: 500,
          bytes_total: 1000,
          bytes_per_sec: 100,
          eta_secs: 5,
          detail: "enviando",
          start_layer: 0,
          end_layer: 13,
        },
      ],
    };
    renderScreen();
    expect(screen.getByRole("table")).toBeTruthy();
    expect(screen.getByText("500 B de 1000 B")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Cancelar" })).toBeTruthy();
  });

  it("shows a finished transfer's state in its own table row", () => {
    transfersState.value = {
      jobs: [
        {
          id: "job-2",
          state: "done",
          direction: "receiving",
          name: "abc123.gguf",
          peer_host: "atena",
          bytes_done: 1000,
          bytes_total: 1000,
          detail: "recebido",
          start_layer: 0,
          end_layer: 13,
        },
      ],
    };
    renderScreen();
    expect(screen.getByText("Terminadas")).toBeTruthy();
    expect(screen.getByText("concluída")).toBeTruthy();
  });
});
