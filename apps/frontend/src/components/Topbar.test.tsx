// @vitest-environment jsdom
/**
 * O estado da criptografia do cluster tem que ser legível de qualquer tela.
 *
 * O que se afirma aqui é que o selo diz as duas coisas - cifrado e não cifrado -
 * em vez de aparecer só quando está ligado. Um selo que some quando o TLS cai é
 * pior que nenhum: o operador lê a ausência como "a tela não mostra isso", não
 * como "o cluster está em texto claro".
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { fakeNode } from "../test/fixtures";
import type { NodeInfo } from "../lib/types";

let nodeData: NodeInfo | undefined;

vi.mock("../lib/queries", async (original) => ({
  ...(await original<typeof import("../lib/queries")>()),
  useNode: () => ({ data: nodeData }),
}));

const { TlsChip } = await import("./TlsIndicator");

function mount() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
          <TlsChip />
        </MemoryRouter>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("TlsChip", () => {
  afterEach(cleanup);

  it("distingue TLS ligado só pelo ícone, com nome acessível", () => {
    nodeData = fakeNode({ tls_required: true });
    mount();
    const indicator = screen.getByTestId("tls-indicator");
    expect(indicator.getAttribute("data-tls-state")).toBe("on");
    expect(indicator.getAttribute("aria-label")).toMatch(/cifrada/i);
    expect(screen.queryByText("TLS")).toBeNull();
  });

  it("distingue texto claro só pelo ícone, com nome acessível", () => {
    nodeData = fakeNode({ tls_required: false });
    mount();
    const indicator = screen.getByTestId("tls-indicator");
    expect(indicator.getAttribute("data-tls-state")).toBe("off");
    expect(indicator.getAttribute("aria-label")).toMatch(/texto claro/i);
    expect(screen.queryByText(/sem TLS/i)).toBeNull();
  });

  it("leva a Configurações / Preferências, que é onde se muda isso", () => {
    nodeData = fakeNode({ tls_required: true });
    const { container } = mount();
    expect(container.querySelector('a[href="/config/preferencias"]')).toBeTruthy();
  });

  it("não renderiza nada enquanto o nó não respondeu", () => {
    nodeData = undefined;
    const { container } = mount();
    expect(container.textContent).toBe("");
  });
});
