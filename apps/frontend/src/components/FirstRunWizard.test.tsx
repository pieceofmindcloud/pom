// @vitest-environment jsdom
/**
 * The first run wizard asks the machine's name, then the network's, and
 * hands over to the hints. Each screen saves through the node before the
 * next one shows.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { FirstRunWizard, useFirstRunWizard } from "./FirstRunWizard";
import { LocaleProvider } from "../i18n/provider";

const STEPS = ["machine_name", "network_name", "bootstrap", "download_model", "deploy_model", "chat", "join"];
const NETWORK = { public_name: "lab-host", visibility: "local", organization: "" };

vi.mock("../lib/queries", () => ({
  useNode: () => ({ data: { node_name: null, hostname: "lab-host" } }),
  useInvalidate: () => () => undefined,
}));

function serve(done: string[] = [], failOn?: string) {
  const state = { active: true, steps: STEPS, done: [...done] };
  const mock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
    const path = String(url);
    const json = (body: unknown, status = 200) =>
      new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
    if (failOn && path.includes(failOn)) return json({ error: "disk full" }, 400);
    if (path.includes("/api/ui/guide")) {
      if (init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { done?: string };
        if (body.done && !state.done.includes(body.done)) state.done.push(body.done);
      }
      return json(state);
    }
    if (path.includes("/api/ui/network/info")) return json({ network: NETWORK, can_edit: true });
    if (path.includes("/api/ui/network/settings")) {
      return json({ network: JSON.parse(String(init?.body)), can_edit: true });
    }
    return json({ node_name: "x", hostname: "x" });
  });
  vi.stubGlobal("fetch", mock);
  return mock;
}

function Harness() {
  const wizard = useFirstRunWizard(true);
  if (wizard.loading) return null;
  return wizard.step ? <FirstRunWizard step={wizard.step} /> : <div data-testid="panel" />;
}

function mount() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="en">
        <Harness />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

function posts(mock: ReturnType<typeof serve>) {
  return mock.mock.calls
    .filter(([, init]) => init?.method === "POST")
    .map(([url, init]) => [String(url).replace(/^.*(\/api\/)/, "$1"), JSON.parse(String(init?.body))]);
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("FirstRunWizard", () => {
  it("walks the machine name, the network name and the handover to the hints", async () => {
    const fetchMock = serve();
    mount();
    const machine = (await screen.findByTestId("wizard-machine-name")) as HTMLInputElement;
    await waitFor(() => expect(machine.value).toBe("lab-host"));
    fireEvent.change(machine, { target: { value: "  workbench  " } });
    fireEvent.click(screen.getByText("Continue"));

    const network = (await screen.findByTestId("wizard-network-name")) as HTMLInputElement;
    expect(network.value).toBe("lab-host");
    expect(screen.getByRole("note").textContent).toContain("advertised to everyone");
    fireEvent.click(screen.getByText("Random"));
    expect(network.value).toMatch(/^[a-z]+-[a-z]+-\d{2}$/);
    fireEvent.change(network, { target: { value: "home-lab" } });
    fireEvent.click(screen.getByText("Continue"));

    await screen.findByText("Follow the instructions on screen");
    fireEvent.click(screen.getByText("Start"));
    await screen.findByTestId("panel");

    expect(posts(fetchMock)).toEqual([
      ["/api/ui/network/node-name", { node_name: "workbench" }],
      ["/api/ui/guide", { done: "machine_name" }],
      ["/api/ui/network/settings", { ...NETWORK, public_name: "home-lab" }],
      ["/api/ui/guide", { done: "network_name" }],
      ["/api/ui/guide", { done: "bootstrap" }],
    ]);
  });

  it("resumes where the operator stopped and lets them go back", async () => {
    serve(["machine_name"]);
    mount();
    await screen.findByTestId("wizard-network-name");
    fireEvent.click(screen.getByText("Back"));
    await screen.findByTestId("wizard-machine-name");
  });

  it("stays on the screen when the save fails, and refuses an empty name", async () => {
    const fetchMock = serve([], "/api/ui/network/node-name");
    mount();
    const machine = (await screen.findByTestId("wizard-machine-name")) as HTMLInputElement;
    await waitFor(() => expect(machine.value).toBe("lab-host"));
    fireEvent.click(screen.getByText("Continue"));
    expect((await screen.findByRole("alert")).textContent).toContain("Could not save");
    expect(screen.queryByTestId("wizard-network-name")).toBeNull();
    expect(posts(fetchMock).some(([url]) => url === "/api/ui/guide")).toBe(false);

    fireEvent.change(machine, { target: { value: "   " } });
    expect((screen.getByText("Continue") as HTMLButtonElement).disabled).toBe(true);
  });
});
