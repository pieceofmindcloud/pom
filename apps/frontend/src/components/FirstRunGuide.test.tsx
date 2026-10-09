// @vitest-environment jsdom
/**
 * The first run guide shows one hint at a time and moves on by itself: the
 * step comes from the node, and where the hint points comes from the screen
 * the operator is on.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { FirstRunGuide } from "./FirstRunGuide";
import { LocaleProvider } from "../i18n/provider";

const WIZARD = ["names", "bootstrap"];
const STEPS = [...WIZARD, "download_model", "deploy_model", "chat", "join"];

function serve(done: string[], active = true, models: Array<{ name: string; path: string }> = []) {
  const state = { active, steps: STEPS, done: [...done] };
  const mock = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
    if (!String(url).includes("/api/ui/guide")) {
      // What the self-ending steps watch: the catalog, the jobs, the cluster.
      return new Response(JSON.stringify({ models, jobs: [], metadata_complete: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }
    if (init?.method === "POST") {
      const body = JSON.parse(String(init.body)) as { done?: string; dismiss?: boolean };
      if (body.dismiss) state.active = false;
      if (body.done && !state.done.includes(body.done)) state.done.push(body.done);
      if (STEPS.every((step) => state.done.includes(step))) state.active = false;
    }
    return new Response(JSON.stringify(state), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", mock);
  return mock;
}

function mount(path: string) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="en">
        <MemoryRouter initialEntries={[path]}>
          <FirstRunGuide />
        </MemoryRouter>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("FirstRunGuide", () => {
  it("stays out of the way while the wizard asks the names", async () => {
    const fetchMock = serve([]);
    mount("/");
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    await act(async () => {});
    expect(screen.queryByTestId("first-run-guide")).toBeNull();
  });

  it("sends the operator to the screen, then to the field on it", async () => {
    serve(WIZARD);
    const first = mount("/");
    const hint = await screen.findByTestId("first-run-guide");
    expect(hint.getAttribute("data-guide-step")).toBe("download_model");
    expect(hint.textContent).toContain("Step 1 of 4");
    expect(hint.textContent).toContain("Find HF models");
    first.unmount();

    mount("/modelos/remotos");
    const there = await screen.findByTestId("first-run-guide");
    expect(there.textContent).toContain("Search for Qwen/Qwen2.5-0.5B-Instruct-GGUF");
  });

  it("moves to the next step when one is skipped and ends after the last", async () => {
    const fetchMock = serve([...WIZARD, "download_model", "deploy_model"]);
    mount("/");
    const hint = await screen.findByTestId("first-run-guide");
    expect(hint.getAttribute("data-guide-step")).toBe("chat");

    act(() => {
      fireEvent.click(screen.getByTestId("first-run-guide-skip"));
    });
    await waitFor(() =>
      expect(screen.getByTestId("first-run-guide").getAttribute("data-guide-step")).toBe("join"),
    );
    expect(screen.getByTestId("first-run-guide-skip").textContent).toBe("Finish");

    act(() => {
      fireEvent.click(screen.getByTestId("first-run-guide-skip"));
    });
    await waitFor(() => expect(screen.queryByTestId("first-run-guide")).toBeNull());
    const posts = fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");
    expect(posts.map(([, init]) => JSON.parse(String(init?.body)))).toEqual([
      { done: "chat" },
      { done: "join" },
    ]);
  });

  it("ends the download step by itself once the model is on disk", async () => {
    const fetchMock = serve(WIZARD, true, [
      { name: "qwen2.5-0.5b-instruct-q4_k_m", path: "/models/qwen2.5-0.5b-instruct-q4_k_m.gguf" },
    ]);
    mount("/");
    await waitFor(() =>
      expect(screen.getByTestId("first-run-guide").getAttribute("data-guide-step")).toBe("deploy_model"),
    );
    const posts = fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");
    expect(posts.map(([, init]) => JSON.parse(String(init?.body)))).toEqual([{ done: "download_model" }]);
  });

  it("follows a step that takes several clicks", async () => {
    const fetchMock = serve(WIZARD);
    const rect = vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({
      left: 10, top: 10, right: 110, bottom: 40, width: 100, height: 30, x: 10, y: 10, toJSON: () => ({}),
    });
    const row = document.createElement("button");
    row.setAttribute("data-guide-repo", "qwen/qwen2.5-0.5b-instruct-gguf");
    document.body.append(row);
    mount("/modelos/remotos");
    const hint = await screen.findByTestId("first-run-guide");
    expect(hint.getAttribute("data-guide-step")).toBe("download_model");
    await waitFor(() => expect(hint.textContent).toContain("This is the one"));

    // The file list opens over the row: the balloon moves on to it.
    const file = document.createElement("button");
    file.setAttribute("data-guide", "model-file");
    document.body.append(file);
    await waitFor(() => expect(screen.getByTestId("first-run-guide").textContent).toContain("Pick a file"), {
      timeout: 2000,
    });
    row.remove();
    file.remove();
    rect.mockRestore();
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
  });

  it("shows nothing on a machine whose guide ended, and hides for good when asked", async () => {
    serve(WIZARD, false);
    const ended = mount("/");
    await waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalled());
    expect(screen.queryByTestId("first-run-guide")).toBeNull();
    ended.unmount();

    const fetchMock = serve(WIZARD);
    mount("/");
    await screen.findByTestId("first-run-guide");
    act(() => {
      fireEvent.click(screen.getByText("Hide these hints"));
    });
    await waitFor(() => expect(screen.queryByTestId("first-run-guide")).toBeNull());
    const posts = fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");
    expect(JSON.parse(String(posts[0][1]?.body))).toEqual({ dismiss: true });
  });
});
