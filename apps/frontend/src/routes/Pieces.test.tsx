// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { Pieces } from "./Pieces";
import { PiecesProvider } from "../components/pieces/PiecesProvider";
import { Dock } from "../components/pieces/Dock";

const manifest = (code: string) => ({ schema: "pom-plugin-ui/v1", plugin_code: code, menu: [], routes: [], screens: {}, assets: [] });

const mocks = vi.hoisted(() => ({
  openPlugin: vi.fn(),
  closePlugin: vi.fn(),
  setPluginLifecycle: vi.fn(),
  refetch: vi.fn(),
  plugins: [] as unknown[],
  node: { plugin_services: [], config: null } as unknown,
  catalog: [] as unknown[],
}));

const menu = vi.hoisted(() => ({ items: [] as Array<{ code: string; to: string; id: string }> }));
vi.mock("../lib/pluginUi", async (original) => {
  const actual = await original<typeof import("../lib/pluginUi")>();
  return { ...actual, pluginMenuItems: () => menu.items };
});
vi.mock("../lib/api", () => ({
  openPlugin: mocks.openPlugin,
  closePlugin: mocks.closePlugin,
  setPluginLifecycle: mocks.setPluginLifecycle,
}));
vi.mock("../lib/queries", () => ({
  usePluginUi: () => ({ data: { plugins: mocks.plugins }, isLoading: false, refetch: mocks.refetch }),
  useNode: () => ({ data: mocks.node }),
  usePublicPluginCatalog: () => ({ data: { plugins: mocks.catalog } }),
}));

function mount() {
  return render(
    <LocaleProvider initialLocale="en">
      <MemoryRouter initialEntries={["/pieces"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <PiecesProvider>
          <Routes>
            <Route path="/pieces" element={<Pieces />} />
            <Route path="/hermes/main" element={<p>hermes screen</p>} />
          </Routes>
          <Dock />
        </PiecesProvider>
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
  it("draws a desktop: available apps on it, open ones on the dock, resident ones pinned before a divider", () => {
    mocks.catalog = [{ code: "logs", display_name: "Business Pack", icon_url: "https://store.example/logs.png" }];
    menu.items = [{ code: "logs", id: "logs", to: "/logs" }, { code: "goose", id: "goose", to: "/goose/main" }];
    mocks.plugins = [
      { code: "goose", version: "0.1.0", manifest: manifest("goose"), on_demand: true, running: true, memory_bytes: 314572800 },
      { code: "logs", version: "1.0.0", manifest: manifest("logs"), on_demand: false, running: true, memory_bytes: 34603008 },
      { code: "hermes", version: "0.2.0", manifest: manifest("hermes"), on_demand: true, running: false },
    ];
    mount();
    expect(screen.getByTestId("puzzle-backdrop")).toBeTruthy();
    const dock = screen.getByTestId("services-dock");
    const items = Array.from(dock.querySelectorAll(":scope > li")).map((li) => li.getAttribute("data-testid"));
    expect(items).toEqual(["background-service-logs", "dock-divider", "background-service-goose"]);
    const logs = screen.getByTestId("background-service-logs");
    expect(within(logs).getByText("Business Pack")).toBeTruthy();
    expect(logs.querySelector("img")?.getAttribute("src")).toBe("https://store.example/logs.png");
    // The labels left the icons: no memory, no "always on" on the desktop.
    expect(screen.queryByText("300 MB")).toBeNull();
    expect(screen.queryByText("always on")).toBeNull();
    const desktop = screen.getByRole("region", { name: "Available" });
    expect(within(desktop).getByText("Hermes")).toBeTruthy();
    mocks.catalog = [];
    menu.items = [];
  });

  it("keeps the memory and the served model in the app's settings window", () => {
    mocks.plugins = [
      { code: "vllm", version: "1.2.0", manifest: manifest("vllm"), on_demand: true, running: true, memory_bytes: 314572800 },
    ];
    mocks.node = {
      plugin_services: [{ plugin_code: "vllm", id: "vllm", kind: "inference", name: "vLLM", status: "ready" }],
      config: { model_label: "Qwen3", inference_service: "vllm" },
    };
    mount();
    fireEvent.click(screen.getByRole("button", { name: "Settings of Vllm" }));
    expect(within(screen.getByTestId("app-settings")).getByText("300 MB")).toBeTruthy();
    // Stopping the engine that serves the model warns first.
    fireEvent.click(screen.getByRole("button", { name: "Close Vllm" }));
    expect(screen.getByText(/releases that model first/)).toBeTruthy();
    mocks.node = { plugin_services: [], config: null };
  });

  it("opens a closed app with a click, which moves it to the dock", async () => {
    mocks.plugins = [{ code: "hermes", version: "0.2.0", manifest: manifest("hermes"), on_demand: true, running: false }];
    mocks.openPlugin.mockResolvedValue({ code: "hermes", running: true });
    mount();
    fireEvent.click(screen.getByRole("button", { name: "Open Hermes" }));
    await waitFor(() => expect(mocks.openPlugin).toHaveBeenCalledWith("hermes"));
    await waitFor(() => expect(mocks.refetch).toHaveBeenCalled());
  });

  it("goes to an open app's screen from the dock", async () => {
    menu.items = [{ code: "hermes", id: "hermes", to: "/hermes/main" }];
    mocks.plugins = [{ code: "hermes", version: "0.2.0", manifest: manifest("hermes"), on_demand: true, running: true }];
    mount();
    expect(within(screen.getByTestId("services-dock")).getByRole("button", { name: "Settings of Hermes" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Open Hermes's screen" }));
    expect(await screen.findByText("hermes screen")).toBeTruthy();
    menu.items = [];
  });

  it("launches a closed app like a system: on the dock, bouncing, then its screen", async () => {
    menu.items = [{ code: "hermes", id: "hermes", to: "/hermes/main" }];
    mocks.plugins = [{ code: "hermes", version: "0.2.0", manifest: manifest("hermes"), on_demand: true, running: false }];
    mocks.openPlugin.mockResolvedValue({ code: "hermes", running: true });
    mount();
    fireEvent.click(screen.getByRole("button", { name: "Open Hermes" }));
    // At once on the dock, bouncing while it starts.
    const docked = within(screen.getByTestId("services-dock")).getByTestId("background-service-hermes");
    expect(docked.querySelector(".animate-bounce")).toBeTruthy();
    expect(await screen.findByText("hermes screen", {}, { timeout: 3000 })).toBeTruthy();
    menu.items = [];
  });

  it("shows a running app with no screen as a system notification, with settings and close", () => {
    mocks.plugins = [{ code: "vllm", version: "1.2.0", manifest: manifest("vllm"), on_demand: true, running: true, memory_bytes: 3 * 1024 ** 3 }];
    mocks.node = {
      plugin_services: [{ plugin_code: "vllm", id: "vllm", kind: "inference", name: "vLLM", status: "ready" }],
      config: { model_label: "Qwen3", inference_service: "vllm" },
    };
    mount();
    const notes = screen.getByTestId("services-notifications");
    expect(within(notes).getByText("serving Qwen3")).toBeTruthy();
    expect(within(notes).getByTestId("service-memory-vllm").textContent).toBe("3.0 GB RAM");
    expect(within(notes).getByRole("button", { name: "Settings of Vllm" })).toBeTruthy();
    expect(within(notes).getByRole("button", { name: "Close Vllm" })).toBeTruthy();
    expect(within(screen.getByTestId("services-dock")).queryByTestId("background-service-vllm")).toBeNull();
    mocks.node = { plugin_services: [], config: null };
  });

  it("opens an operating-system-like menu on a right click", async () => {
    menu.items = [{ code: "goose", id: "goose", to: "/goose/main" }];
    mocks.plugins = [{ code: "goose", version: "0.1.0", manifest: manifest("goose"), on_demand: true, running: true, lifecycle_default: "on_demand" }];
    mocks.setPluginLifecycle.mockResolvedValue({});
    mount();
    fireEvent.contextMenu(screen.getByTestId("background-service-goose"), { clientX: 100, clientY: 100 });
    const contextMenu = screen.getByTestId("app-context-menu");
    const labels = within(contextMenu).getAllByRole("menuitem").map((item) => item.textContent);
    expect(labels).toEqual(["Go to screen", "Settings…", "Keep always on", "Close"]);
    fireEvent.click(within(contextMenu).getByRole("menuitem", { name: "Keep always on" }));
    await waitFor(() => expect(mocks.setPluginLifecycle).toHaveBeenCalledWith("goose", "resident"));
    expect(screen.queryByTestId("app-context-menu")).toBeNull();
    menu.items = [];
  });

  it("lets the operator drag an available app anywhere on the desktop and keeps it there", () => {
    // jsdom has no PointerEvent: a MouseEvent carries the coordinates.
    if (!("PointerEvent" in window)) {
      class PointerEventShim extends MouseEvent {
        pointerId: number;
        constructor(type: string, init: PointerEventInit = {}) {
          super(type, init);
          this.pointerId = init.pointerId ?? 1;
        }
      }
      (window as unknown as { PointerEvent: typeof MouseEvent }).PointerEvent = PointerEventShim;
    }
    mocks.plugins = [{ code: "hermes", version: "0.2.0", manifest: manifest("hermes"), on_demand: true, running: false }];
    mount();
    const desk = screen.getByRole("region", { name: "Available" });
    desk.getBoundingClientRect = () => ({ left: 0, top: 0, width: 1000, height: 800, right: 1000, bottom: 800, x: 0, y: 0, toJSON: () => ({}) });
    const icon = screen.getByTestId("background-service-hermes");
    fireEvent.pointerDown(icon, { button: 0, clientX: 40, clientY: 40, pointerId: 1 });
    fireEvent.pointerMove(icon, { clientX: 340, clientY: 240, pointerId: 1 });
    fireEvent.pointerUp(icon, { pointerId: 1 });
    expect(icon.style.left).toBe("324px");
    expect(icon.style.top).toBe("224px");
    expect(window.localStorage.getItem(Object.keys(window.localStorage).find((k) => k.endsWith("services.desktop")) ?? "")).toContain("hermes");
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

  it("opens an app's settings from its icon, where a stopped one is started", async () => {
    mocks.plugins = [{ code: "hermes", version: "0.2.0", manifest: manifest("hermes"), on_demand: true, running: false }];
    mocks.openPlugin.mockResolvedValue({ code: "hermes", running: true });
    mount();
    fireEvent.click(screen.getByRole("button", { name: "Settings of Hermes" }));
    expect(screen.getByTestId("app-settings")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Open" }));
    await waitFor(() => expect(mocks.openPlugin).toHaveBeenCalledWith("hermes"));
  });

  it("lets the operator turn an always-on plugin on demand, and give the choice back", async () => {
    mocks.plugins = [{
      code: "logs", version: "1.0.0", manifest: manifest("logs"), on_demand: false, running: true,
      lifecycle_default: "resident",
    }];
    mocks.setPluginLifecycle.mockResolvedValue({ code: "logs", on_demand: true, running: true });
    mount();
    fireEvent.click(screen.getByRole("button", { name: "Settings of Logs" }));
    expect(screen.getByTestId("lifecycle-resident").getAttribute("aria-checked")).toBe("true");
    expect(within(screen.getByTestId("lifecycle-resident")).getByText("plugin default")).toBeTruthy();
    fireEvent.click(screen.getByTestId("lifecycle-on_demand"));
    await waitFor(() => expect(mocks.setPluginLifecycle).toHaveBeenCalledWith("logs", "on_demand"));
    await waitFor(() => expect(mocks.refetch).toHaveBeenCalled());
    cleanup();
    // Overridden: picking the plugin's default clears the override (null).
    mocks.plugins = [{
      code: "logs", version: "1.0.0", manifest: manifest("logs"), on_demand: true, running: true,
      lifecycle_default: "resident", lifecycle_override: "on_demand",
    }];
    mount();
    fireEvent.click(screen.getByRole("button", { name: "Settings of Logs" }));
    fireEvent.click(screen.getByTestId("lifecycle-resident"));
    await waitFor(() => expect(mocks.setPluginLifecycle).toHaveBeenCalledWith("logs", null));
  });

  it("says so when no plugin is installed", () => {
    mocks.plugins = [];
    mount();
    expect(screen.getByText("No plugin is installed on this machine.")).toBeTruthy();
  });
});
