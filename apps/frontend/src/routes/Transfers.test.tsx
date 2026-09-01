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

vi.mock("../components/ModelDownloadsPanel", () => ({
  ModelDownloadsPanel: () => <div data-testid="model-downloads-panel">model downloads</div>,
}));
vi.mock("../lib/api", () => ({ api: { post: vi.fn() } }));
vi.mock("../lib/queries", () => ({
  useModelDownloads: vi.fn(() => queryResult([])),
  useOffers: vi.fn(() => queryResult({ offers: [] })),
  useTransfers: vi.fn(() => queryResult({ jobs: [] })),
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), error: vi.fn(), info: vi.fn(), fromError: vi.fn() }),
}));

afterEach(cleanup);

describe("Transfers", () => {
  it("shows remote model downloads in the in-progress screen", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
          <Transfers />
        </MemoryRouter>
      </LocaleProvider>,
    );

    expect(screen.getByTestId("model-downloads-panel")).toBeTruthy();
  });
});
