// @vitest-environment jsdom
/**
 * As duas promessas desta tela: perguntar antes de alcançar o cluster inteiro,
 * e contar máquina a máquina o que aconteceu. A segunda é a que mais custa se
 * quebrar em silêncio — um relatório que arredonda para "pronto!" manda o
 * operador embora achando que liberou memória que continua presa.
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

/** Um clique, com o React já tendo processado o que ele disparou. */
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
    // A pergunta ainda está aberta: nada saiu.
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
    // O motivo da máquina que falhou aparece literalmente: é por onde o
    // operador descobre em qual caixa a memória continua presa.
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
