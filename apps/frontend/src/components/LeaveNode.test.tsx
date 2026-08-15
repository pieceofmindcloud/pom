// @vitest-environment jsdom
/**
 * O card manda a maquina certa embora, e so depois de perguntar.
 *
 * O `node_id` no corpo e o que separa "esta maquina sai" de "aquela sai": sem
 * ele o no local se desliga, e o operador que clicou no card do peer ficaria
 * sem painel e com o peer ainda no ar.
 *
 * A confirmacao e botao proprio "Confirmar saída", separado do gatilho "Sair
 * da rede" - os dois nao compartilham mais o mesmo rotulo.
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

  it("pergunta nomeando a maquina, e nao desliga ninguem antes da resposta", () => {
    const fetchMock = okFetch();
    mount(false);

    click(screen.getByRole("button", { name: "Sair da rede" }));

    expect(screen.getByText(/Tirar ksia da rede\?/i)).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("no card de um peer, manda o node_id daquela maquina", async () => {
    const fetchMock = okFetch();
    mount(false);

    click(screen.getByRole("button", { name: "Sair da rede" }));
    click(screen.getByRole("button", { name: "Confirmar saída" }));

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("/api/ui/leave");
    expect(JSON.parse(init.body as string)).toEqual({ node_id: "no-b" });
  });

  it("no proprio card, o pedido e local -- sem node_id, sem repasse", async () => {
    const fetchMock = okFetch();
    mount(true);

    click(screen.getByRole("button", { name: "Sair da rede" }));
    click(screen.getByRole("button", { name: "Confirmar saída" }));

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(JSON.parse(init.body as string)).toEqual({});
  });
});