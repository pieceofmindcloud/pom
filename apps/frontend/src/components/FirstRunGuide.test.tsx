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

const STEPS = ["network_name", "machine_name", "join"];

function serve(done: string[], active = true) {
  const state = { active, steps: STEPS, done: [...done] };
  const mock = vi.fn(async (_url: RequestInfo | URL, init?: RequestInit) => {
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
  it("sends the operator to the screen, then to the field on it", async () => {
    serve([]);
    const first = mount("/");
    const hint = await screen.findByTestId("first-run-guide");
    expect(hint.getAttribute("data-guide-step")).toBe("network_name");
    expect(hint.textContent).toContain("Step 1 of 3");
    expect(hint.textContent).toContain("Open Network, then Network information.");
    first.unmount();

    mount("/config/rede");
    const there = await screen.findByTestId("first-run-guide");
    expect(there.textContent).toContain("Give it a name of its own and save.");
  });

  it("moves to the next step when one is skipped and ends after the last", async () => {
    const fetchMock = serve(["network_name"]);
    mount("/");
    const hint = await screen.findByTestId("first-run-guide");
    expect(hint.getAttribute("data-guide-step")).toBe("machine_name");

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
      { done: "machine_name" },
      { done: "join" },
    ]);
  });

  it("shows nothing on a machine whose guide ended, and hides for good when asked", async () => {
    serve([], false);
    const ended = mount("/");
    await waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalled());
    expect(screen.queryByTestId("first-run-guide")).toBeNull();
    ended.unmount();

    const fetchMock = serve([]);
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
