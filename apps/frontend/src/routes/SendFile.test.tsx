// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { SendFilePanel } from "./SendFile";

vi.mock("../lib/api", () => ({ api: { post: vi.fn() } }));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));
vi.mock("../lib/queries", () => ({
  useCluster: () => ({
    data: {
      self_id: "ksia",
      peers: [
        { node_id: "atena", is_self: false, short_id: "b2c3", addr: "10.0.0.2:8080", node: { hostname: "atena" } },
      ],
    },
    isLoading: false,
  }),
  useLocalCatalog: () => ({
    data: {
      models: [
        {
          id: "model-1",
          name: "qwen.gguf",
          path: "/models/qwen.gguf",
          size_bytes: 1,
          source: "folder",
          source_label: "Folder",
        },
      ],
      shard_sets: [],
    },
    isLoading: false,
  }),
}));

afterEach(cleanup);

function renderScreen(initialEntry = "/transfers") {
  return render(
    <MemoryRouter
      initialEntries={[initialEntry]}
      future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
    >
      <LocaleProvider initialLocale="en">
        <SendFilePanel />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("SendFilePanel", () => {
  it("lists the local catalog and known peers to build an offer", () => {
    renderScreen();
    expect(screen.getByText(/qwen\.gguf/)).toBeTruthy();
    expect(screen.getByText(/atena/)).toBeTruthy();
  });

  it("preselects the filename passed in the query string", () => {
    renderScreen("/transfers?filename=qwen.gguf");
    const select = screen.getAllByRole("combobox")[0] as HTMLSelectElement;
    expect(select.value).toBe("qwen.gguf");
  });
});
