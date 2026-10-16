// @vitest-environment jsdom
/**
 * The two promises of this screen: ask before reaching the whole cluster,
 * and count machine by machine what happened. The second is the one that costs most if it
 * breaks silently - a report that rounds off to "done!" sends the
 * operator away thinking they freed memory that is still held.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { UndeployPipeline } from "./UndeployPipeline";
import { LocaleProvider } from "../i18n/provider";

const targets = [
  { node_id: "no-a", hostname: "ksia", start_layer: 0, end_layer: 15 },
  { node_id: "no-b", hostname: "macbook", start_layer: 16, end_layer: 31 },
];

function mount() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <UndeployPipeline group="qwen35" targets={targets} />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

/** One click, with React having already processed what it triggered. */
function click(el: Element) {
  act(() => {
    fireEvent.click(el);
  });
}

function answer(body: unknown) {
  return vi.fn(async () =>
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

describe("UndeployPipeline", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it("names the machines before asking, and sends nothing without the click", async () => {
    const fetchMock = answer({});
    vi.stubGlobal("fetch", fetchMock);
    mount();

    click(screen.getByRole("button", { name: "Desimplantar" }));

    expect(screen.getByText("ksia")).toBeTruthy();
    expect(screen.getByText("macbook")).toBeTruthy();
    expect(screen.getByText("L0–15")).toBeTruthy();
    expect(screen.getByText("L16–31")).toBeTruthy();
    // The question is still open: nothing went out.
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("reports each machine, and the one that did not confirm does not become a success", async () => {
    const fetchMock = answer({
      group: "qwen35",
      released: 1,
      failed: 1,
      results: [
        {
          node_id: "no-a",
          hostname: "ksia",
          start_layer: 0,
          end_layer: 15,
          released: true,
        },
        {
          node_id: "no-b",
          hostname: "macbook",
          start_layer: 16,
          end_layer: 31,
          released: false,
          error: "máquina inacessível: connection refused",
        },
      ],
    });
    vi.stubGlobal("fetch", fetchMock);
    mount();

    click(screen.getByRole("button", { name: "Desimplantar" }));
    click(screen.getByRole("button", { name: "Desimplantar em todas" }));

    await waitFor(() => expect(screen.getByText(/não confirmaram/i)).toBeTruthy());
    // The failed machine's reason appears literally: it is how the
    // operator finds out in which box the memory is still held.
    expect(screen.getByText(/connection refused/i)).toBeTruthy();
    expect(screen.getByText(/^liberada$/i)).toBeTruthy();

    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("/api/ui/undeploy");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body as string)).toEqual({ group: "qwen35" });
  });

  it("with all released, the report says so and invents no failure", async () => {
    vi.stubGlobal(
      "fetch",
      answer({
        group: "qwen35",
        released: 2,
        failed: 0,
        results: targets.map((m) => ({ ...m, released: true })),
      }),
    );
    mount();

    click(screen.getByRole("button", { name: "Desimplantar" }));
    click(screen.getByRole("button", { name: "Desimplantar em todas" }));

    await waitFor(() =>
      expect(screen.getByText(/2 máquinas liberaram a memória/i)).toBeTruthy(),
    );
    expect(screen.queryByText(/não confirmaram/i)).toBeNull();
  });
});
