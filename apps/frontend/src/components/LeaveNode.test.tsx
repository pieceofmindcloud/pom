// @vitest-environment jsdom
/**
 * O card manda a máquina certa embora, e só depois de perguntar.
 *
 * O `node_id` no corpo é o que separa "esta máquina sai" de "aquela sai": sem
 * ele o nó local se desliga, e o operador que clicou no card do peer ficaria
 * sem painel e com o peer ainda no ar.
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

  it("pergunta nomeando a máquina, e não desliga ninguém antes da resposta", () => {
    const fetchMock = okFetch();
    mount(false);

    click(screen.getByRole("button", { name: "Sair da rede" }));

    expect(screen.getByText(/Tirar ksia da rede\?/i)).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("no card de um peer, manda o node_id daquela máquina", async () => {
    const fetchMock = okFetch();
    mount(false);

    click(screen.getByRole("button", { name: "Sair da rede" }));
    // Dois botões com este nome enquanto a confirmação está aberta: o do card e
    // o de confirmar. O último no DOM é o da janela.
    const buttons = screen.getAllByRole("button", { name: "Sair da rede" });
    click(buttons[buttons.length - 1]);

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("/api/ui/leave");
    expect(JSON.parse(init.body as string)).toEqual({ node_id: "no-b" });
  });

  it("no próprio card, o pedido é local — sem node_id, sem repasse", async () => {
    const fetchMock = okFetch();
    mount(true);

    click(screen.getByRole("button", { name: "Sair da rede" }));
    const buttons = screen.getAllByRole("button", { name: "Sair da rede" });
    click(buttons[buttons.length - 1]);

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(JSON.parse(init.body as string)).toEqual({});
  });
});
