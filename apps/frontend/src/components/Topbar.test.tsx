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

const { TlsChip } = await import("./Topbar");

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

  it("diz que o cluster está cifrado quando a política está ligada", () => {
    nodeData = fakeNode({ tls_required: true });
    mount();
    expect(screen.getByText("TLS")).toBeTruthy();
  });

  it("diz que não está, quando desligada", () => {
    nodeData = fakeNode({ tls_required: false });
    mount();
    expect(screen.getByText(/sem TLS/i)).toBeTruthy();
  });

  it("leva a Configurações / Rede, que é onde se muda isso", () => {
    nodeData = fakeNode({ tls_required: true });
    const { container } = mount();
    expect(container.querySelector('a[href="/config/rede"]')).toBeTruthy();
  });

  it("não renderiza nada enquanto o nó não respondeu", () => {
    nodeData = undefined;
    const { container } = mount();
    expect(container.textContent).toBe("");
  });
});
