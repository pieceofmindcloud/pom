// @vitest-environment jsdom
/**
 * The card sends the right machine away, and only after asking.
 *
 * The `node_id` in the body is what separates "this machine leaves" from "that one leaves": without
 * it the local node shuts itself down, and the operator who clicked on the peer's card would be
 * left without a panel and with the peer still up.
 *
 * The confirmation is its own button "Confirmar saída", separate from the trigger "Sair
 * da rede" - the two no longer share the same label.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LeaveNode } from "./LeaveNode";
import { LocaleProvider } from "../i18n/provider";

function mount(isSelf: boolean) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <LeaveNode nodeId="no-b" hostname="ksia" isSelf={isSelf} />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

function click(el: Element) {
  act(() => {
    fireEvent.click(el);
  });
}

function okFetch() {
  const mock = vi.fn(async () =>
    new Response(JSON.stringify({ detached: true }), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }),
  );
  vi.stubGlobal("fetch", mock);
  return mock;
}

describe("LeaveNode", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it("asks naming the machine, and turns nobody off before the answer", () => {
    const fetchMock = okFetch();
    mount(false);

    click(screen.getByRole("button", { name: "Sair da rede" }));

    expect(screen.getByText(/Tirar ksia da rede\?/i)).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("on a peer card, sends the node_id of that machine", async () => {
    const fetchMock = okFetch();
    mount(false);

    click(screen.getByRole("button", { name: "Sair da rede" }));
    click(screen.getByRole("button", { name: "Confirmar saída" }));

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("/api/ui/leave");
    expect(JSON.parse(init.body as string)).toEqual({ node_id: "no-b" });
  });

  it("on its own card, the request is local -- no node_id, no forwarding", async () => {
    const fetchMock = okFetch();
    mount(true);

    click(screen.getByRole("button", { name: "Sair da rede" }));
    click(screen.getByRole("button", { name: "Confirmar saída" }));

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(JSON.parse(init.body as string)).toEqual({});
  });
});