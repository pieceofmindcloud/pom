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

describe("Services", () => {
  it("shows open apps on the shelf and stopped ones in the folder, with their names", () => {
    mocks.plugins = [
      { code: "goose", version: "0.1.0", manifest: manifest("goose"), on_demand: true, running: true, memory_bytes: 314572800 },
      { code: "logs", version: "1.0.0", manifest: manifest("logs"), on_demand: false, running: true },
      { code: "hermes", version: "0.2.0", manifest: manifest("hermes"), on_demand: true, running: false },
    ];
    mount();
    const shelf = screen.getByRole("region", { name: "Open (2)" });
    expect(within(shelf).getByText("Goose")).toBeTruthy();
    expect(within(shelf).getByText("300 MB")).toBeTruthy();
    // A resident plugin is an open app too, always on and with no close.
    expect(within(screen.getByTestId("background-service-logs")).getByText("always on")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Close Logs" })).toBeNull();
    const folder = screen.getByRole("region", { name: "Stopped" });
    expect(within(folder).getByText("Hermes")).toBeTruthy();
  });

  it("closes an open app after a confirmation", async () => {
    mocks.plugins = [{ code: "goose", version: "0.1.0", manifest: manifest("goose"), on_demand: true, running: true }];
    mocks.closePlugin.mockResolvedValue({ code: "goose", running: false });
    mount();
    fireEvent.click(screen.getByRole("button", { name: "Close Goose" }));
    expect(mocks.closePlugin).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    await waitFor(() => expect(mocks.closePlugin).toHaveBeenCalledWith("goose"));
    await waitFor(() => expect(mocks.refetch).toHaveBeenCalled());
  });

  it("opens a stopped app with a click on its icon", async () => {
    mocks.plugins = [{ code: "hermes", version: "0.2.0", manifest: manifest("hermes"), on_demand: true, running: false }];
    mocks.openPlugin.mockResolvedValue({ code: "hermes", running: true });
    mount();
    fireEvent.click(screen.getByRole("button", { name: "Open Hermes" }));
    await waitFor(() => expect(mocks.openPlugin).toHaveBeenCalledWith("hermes"));
  });

  it("marks an inference engine, names the model it serves and warns before stopping it", () => {
    mocks.plugins = [
      { code: "vllm", version: "1.2.0", manifest: manifest("vllm"), on_demand: true, running: true, memory_bytes: 1048576 },
    ];
    mocks.node = {
      plugin_services: [{ plugin_code: "vllm", id: "vllm", kind: "inference", name: "vLLM", status: "ready" }],
      config: { model_label: "Qwen3", inference_service: "vllm" },
    };
    mount();
    expect(screen.getByTestId("background-service-detail-vllm").textContent).toBe("serving Qwen3");
    fireEvent.click(screen.getByRole("button", { name: "Close Vllm" }));
    expect(screen.getByText(/releases that model first/)).toBeTruthy();
    mocks.node = { plugin_services: [], config: null };
  });

  it("says so when no plugin is installed", () => {
    mocks.plugins = [];
    mount();
    expect(screen.getByText("No plugin is installed on this machine.")).toBeTruthy();
  });
});
