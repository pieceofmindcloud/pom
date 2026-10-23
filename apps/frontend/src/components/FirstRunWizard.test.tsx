// @vitest-environment jsdom
/**
 * The first run wizard asks one name, given to the machine and to its
 * network, greets the operator and hands over to the hints by itself. The
 * name is saved through the node before the welcome shows.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { FirstRunWizard, useFirstRunWizard } from "./FirstRunWizard";
import { LocaleProvider } from "../i18n/provider";

const STEPS = ["names", "bootstrap", "download_model", "deploy_model", "chat", "join"];
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
  return wizard.step ? <FirstRunWizard step={wizard.step} welcomeMs={20} /> : <div data-testid="panel" />;
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
  it("asks one name for the machine and its network, then welcomes and hands over", async () => {
    const fetchMock = serve();
    mount();
    const name = (await screen.findByTestId("wizard-name")) as HTMLInputElement;
    await waitFor(() => expect(name.value).toBe("lab-host"));
    expect(screen.queryByTestId("wizard-network-name")).toBeNull();
    expect(screen.getByRole("note").textContent).toContain("advertised to everyone");

    fireEvent.click(screen.getByText("Random"));
    expect(name.value).toMatch(/^[a-z]+-[a-z]+-\d{2}$/);

    fireEvent.change(name, { target: { value: "  workbench  " } });
    fireEvent.click(screen.getByText("Continue"));

    expect((await screen.findByTestId("wizard-welcome")).textContent).toContain("Welcome to POM");
    await screen.findByTestId("panel");

    expect(posts(fetchMock)).toEqual([
      ["/api/ui/network/node-name", { node_name: "workbench" }],
      ["/api/ui/network/settings", { ...NETWORK, public_name: "workbench" }],
      ["/api/ui/guide", { done: "names" }],
      ["/api/ui/guide", { done: "bootstrap" }],
    ]);
  });

  it("resumes on the welcome when the name was already saved", async () => {
    const fetchMock = serve(["names"]);
    mount();
    await screen.findByTestId("wizard-welcome");
    await screen.findByTestId("panel");
    expect(posts(fetchMock)).toEqual([["/api/ui/guide", { done: "bootstrap" }]]);
  });

  it("stays on the screen when the save fails, and refuses an empty name", async () => {
    const fetchMock = serve([], "/api/ui/network/settings");
    mount();
    const name = (await screen.findByTestId("wizard-name")) as HTMLInputElement;
    await waitFor(() => expect(name.value).toBe("lab-host"));
    await waitFor(() => expect((screen.getByText("Continue") as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByText("Continue"));
    expect((await screen.findByRole("alert")).textContent).toContain("Could not save");
    expect(screen.getByTestId("wizard-name")).toBeTruthy();
    expect(posts(fetchMock).some(([url]) => url === "/api/ui/guide")).toBe(false);

    fireEvent.change(name, { target: { value: "   " } });
    expect((screen.getByText("Continue") as HTMLButtonElement).disabled).toBe(true);
  });
});
