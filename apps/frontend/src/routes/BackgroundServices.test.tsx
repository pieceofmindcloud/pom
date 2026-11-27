// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { BackgroundServices } from "./BackgroundServices";

const manifest = (code: string) => ({ schema: "pom-plugin-ui/v1", plugin_code: code, menu: [], routes: [], screens: {}, assets: [] });

const mocks = vi.hoisted(() => ({
  openPlugin: vi.fn(),
  closePlugin: vi.fn(),
  refetch: vi.fn(),
  plugins: [] as unknown[],
  node: { plugin_services: [], config: null } as unknown,
}));

vi.mock("../lib/api", () => ({ openPlugin: mocks.openPlugin, closePlugin: mocks.closePlugin }));
vi.mock("../lib/queries", () => ({
  usePluginUi: () => ({ data: { plugins: mocks.plugins }, isLoading: false, refetch: mocks.refetch }),
  useNode: () => ({ data: mocks.node }),
}));

function mount() {
  return render(
    <LocaleProvider initialLocale="en">
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <BackgroundServices />
      </MemoryRouter>
    </LocaleProvider>,
  );
}

afterEach(() => {
  cleanup();
  mocks.openPlugin.mockReset();
  mocks.closePlugin.mockReset();
  mocks.refetch.mockReset();
});

describe("BackgroundServices", () => {
  it("marks an inference engine, names the model it serves and warns before stopping it", async () => {
    mocks.plugins = [
      { code: "vllm", version: "1.2.0", manifest: manifest("vllm"), on_demand: true, running: true, memory_bytes: 1048576 },
    ];
    mocks.node = {
      plugin_services: [{ plugin_code: "vllm", id: "vllm", kind: "inference", name: "vLLM", status: "ready" }],
      config: { model_label: "Qwen3", inference_service: "vllm" },
    };
    mount();
    const row = screen.getByTestId("background-service-vllm");
    expect(within(row).getByText("Inference engine")).toBeTruthy();
    expect(within(row).getByTestId("background-service-model-vllm").textContent).toBe("serving Qwen3");
    fireEvent.click(within(row).getByRole("button", { name: /close/i }));
    expect(screen.getByText(/releases that model first/)).toBeTruthy();
    mocks.node = { plugin_services: [], config: null };
  });

  it("lists running on-demand plugins with memory and leaves resident ones out", () => {
    mocks.plugins = [
      { code: "goose", version: "0.1.0", manifest: manifest("goose"), on_demand: true, running: true, memory_bytes: 314572800 },
      { code: "logs", version: "1.0.0", manifest: manifest("logs"), on_demand: false, running: true },
    ];
    mount();
    const row = screen.getByTestId("background-service-goose");
    expect(within(row).getByText("Goose")).toBeTruthy();
    expect(within(row).getByText("300 MB")).toBeTruthy();
    expect(within(row).getByText("0.1.0")).toBeTruthy();
    expect(screen.queryByTestId("background-service-logs")).toBeNull();
  });

  it("close asks for confirmation then calls close and refreshes", async () => {
    mocks.plugins = [{ code: "goose", version: "0.1.0", manifest: manifest("goose"), on_demand: true, running: true }];
    mocks.closePlugin.mockResolvedValue({ code: "goose", running: false });
    mount();
    fireEvent.click(within(screen.getByTestId("background-service-goose")).getByRole("button", { name: "Close" }));
    expect(mocks.closePlugin).not.toHaveBeenCalled();
    fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(mocks.closePlugin).toHaveBeenCalledWith("goose"));
    await waitFor(() => expect(mocks.refetch).toHaveBeenCalled());
  });

  it("stopped on-demand plugins offer Open", async () => {
    mocks.plugins = [{ code: "goose", version: "0.1.0", manifest: manifest("goose"), on_demand: true, running: false }];
    mocks.openPlugin.mockResolvedValue({ code: "goose", running: true });
    mount();
    const row = screen.getByTestId("background-service-goose");
    expect(within(row).getByText("Stopped")).toBeTruthy();
    fireEvent.click(within(row).getByRole("button", { name: "Open" }));
    await waitFor(() => expect(mocks.openPlugin).toHaveBeenCalledWith("goose"));
  });

  it("says so when nothing runs in the background", () => {
    mocks.plugins = [];
    mount();
    expect(screen.getByText("No plugin runs on demand on this machine.")).toBeTruthy();
  });
});
