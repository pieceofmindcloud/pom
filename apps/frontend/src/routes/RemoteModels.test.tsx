// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { RemoteModels } from "./RemoteModels";

vi.mock("../components/ModelDownloadsPanel", () => ({
  ModelDownloadsPanel: () => <div data-testid="model-downloads-panel" />,
}));

// Stand-ins with the real props, so the test drives the screen's own state:
// choosing a file opens the download dialog over the details.
vi.mock("../components/RemoteModelDetails", () => ({
  RemoteModelDetails: ({ open, onChoose }: { open: boolean; onChoose: (file: unknown, details: unknown) => void }) =>
    open ? (
      <div role="dialog" aria-label="model details">
        <button onClick={() => onChoose({ filename: "q4.gguf" }, { repo_id: "org/qwen-gguf" })}>choose file</button>
      </div>
    ) : null,
}));

vi.mock("../components/DownloadModelDialog", () => ({
  DownloadModelDialog: ({ open, onStarted, onClose }: { open: boolean; onStarted: () => void; onClose: () => void }) =>
    open ? (
      <div role="dialog" aria-label="download">
        <button onClick={() => { onStarted(); onClose(); }}>start download</button>
      </div>
    ) : null,
}));

vi.mock("../lib/queries", () => ({
  keys: { jobs: ["jobs"] },
  useRemoteModelDetails: () => ({
    data: undefined,
    isLoading: false,
    isError: false,
  }),
  useRemoteModelProviderStatus: () => ({
    data: { cli: { installed: true, authenticated: false } },
    isLoading: false,
    isError: false,
    refetch: vi.fn(),
  }),
  useTrendingPublicModels: () => ({
    data: [{
      repo_id: "public/trending-GGUF",
      author: "public",
      description: null,
      downloads: 25,
      likes: 4,
      private: false,
      gated: false,
      tags: ["gguf"],
      last_modified: "2026-09-01T00:00:00Z",
      has_gguf: true,
      installed: false,
      downloading: false,
    }],
    isLoading: false,
    isError: false,
    refetch: vi.fn(),
  }),
  useRemoteModelSearch: () => ({
    data: {
      models: [
        {
          repo_id: "org/qwen-gguf",
          author: "org",
          description: "A quantised Qwen model.",
          has_gguf: true,
          gated: false,
          private: false,
          installed: false,
          downloading: false,
          tags: ["gguf", "chat"],
          downloads: 12_345,
          likes: 42,
          last_modified: "2026-08-01T00:00:00Z",
        },
      ],
      total: 1,
      has_next: false,
    },
    isLoading: false,
    isError: false,
    isFetching: false,
    refetch: vi.fn(),
  }),
}));

afterEach(cleanup);

function renderScreen(initialEntry = "/models/remote?q=qwen") {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter
        initialEntries={[initialEntry]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="en">
          <RemoteModels />
        </LocaleProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("RemoteModels", () => {
  it("renders search results as a full-width table", () => {
    renderScreen();
    expect(screen.getByRole("table")).toBeTruthy();
    expect(screen.getByText("org/qwen-gguf")).toBeTruthy();
    expect(screen.getByText("12,345")).toBeTruthy();
  });

  it("shows public trending models on first load and requires login in the search bar", () => {
    renderScreen("/models/remote");
    expect(screen.getByText("public/trending-GGUF")).toBeTruthy();
    expect(screen.getByText("Trending GGUF models")).toBeTruthy();
    expect(screen.getByText("Log in to Hugging Face to search").closest("a")?.getAttribute("href"))
      .toBe("/config/preferences#hugging-face");
    expect(screen.getByRole("button", { name: "Search" })).toBeTruthy();
    expect(screen.queryByText("Provider status")).toBeNull();
  });

  it("opens the details dialog when a result row is activated", () => {
    renderScreen();
    expect(screen.queryByRole("dialog")).toBeNull();
    fireEvent.click(screen.getByText("org/qwen-gguf"));
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("closes the model details too once a download starts", () => {
    renderScreen();
    fireEvent.click(screen.getByText("org/qwen-gguf"));
    fireEvent.click(screen.getByText("choose file"));
    expect(screen.getByRole("dialog", { name: "download" })).toBeTruthy();
    fireEvent.click(screen.getByText("start download"));
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});
