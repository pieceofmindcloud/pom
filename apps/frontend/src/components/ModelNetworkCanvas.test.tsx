// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { edgeStyle, isControlTarget, ModelNetworkCanvas } from "./ModelNetworkCanvas";
import { LocaleProvider } from "../i18n/provider";
import type { ClusterView } from "../lib/types";
import { fakeCluster, fakeNode, fakePeer, fakeSlicePeer } from "../test/fixtures";

describe("edgeStyle", () => {
  it("pinta a rota principal de verde, sólida e com seta", () => {
    const s = edgeStyle("principal");
    expect(s.className).toContain("stroke-aqua");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeTruthy();
  });

  it("pinta rota paralela de azul, sólida e sem seta", () => {
    const s = edgeStyle("paralela");
    expect(s.className).toContain("stroke-iris");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeUndefined();
  });

  it("pinta rota inativa de cinza tracejado", () => {
    const s = edgeStyle("inativa");
    expect(s.className).toContain("stroke-faint");
    expect(s.strokeDasharray).toBe("7 7");
  });
});

const cluster = fakeCluster([fakeSlicePeer("a", 0, 15), fakeSlicePeer("b", 16, 31)]);

function mount(view: ClusterView = cluster) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <ModelNetworkCanvas cluster={view} />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

/**
 * O card do Fluxo e a aba Detalhes mostram a mesma coisa, e o caminho até ela é
 * o duplo clique. Um clique só continua sendo seleção e arrasto.
 */
describe("ModelNetworkCanvas: detalhes por duplo clique", () => {
  afterEach(cleanup);

  it("nao inclui peer sem fatia no otherRanges da janela de detalhes", () => {
    const b = fakeSlicePeer("b", 0, 0);
    b.total_layers = 0;
    const view = fakeCluster([fakeSlicePeer("a", 5, 10), b], "qwen35");
    mount(view);

    const cards = document.querySelectorAll("article");
    expect(cards.length).toBeGreaterThanOrEqual(1);

    act(() => {
      fireEvent.doubleClick(cards[1]);
    });

    // The dialog shows the serving peer's real range, not the [0,0] that
    // a non-serving peer would contribute to otherRanges without the filter.
    const dialog = screen.getByRole("dialog");
    expect(dialog.textContent).toMatch(/L5.+10/);
  });

  it("nao abre nada antes do duplo clique", () => {
    mount();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("abre a janela da máquina no duplo clique do card", () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.doubleClick(card);
    });
    const dialog = screen.getByRole("dialog");
    expect(dialog.getAttribute("aria-label")).toContain("Detalhes de");
  });

  it("um clique simples não abre a janela", () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.click(card);
    });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  // A janela sai com animação, então "fechou" se afirma esperando a saída — e
  // não no tique seguinte ao clique.
  it("fecha pelo botão e pelo Esc", async () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.doubleClick(card);
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Fechar" }));
    });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    act(() => {
      fireEvent.doubleClick(card);
    });
    expect(screen.getByRole("dialog")).toBeTruthy();
    act(() => {
      fireEvent.keyDown(document, { key: "Escape" });
    });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("mostra a máquina em que se clicou, e não a primeira da lista", () => {
    mount({
      ...cluster,
      peers: [
        fakeSlicePeer("a", 0, 15, { node: fakeNode({ node_id: "a", short_id: "a", hostname: "salem" }) }),
        fakeSlicePeer("b", 16, 31, { node: fakeNode({ node_id: "b", short_id: "b", hostname: "ksia" }) }),
      ],
    });
    const cards = document.querySelectorAll("article");
    act(() => {
      fireEvent.doubleClick(cards[1]);
    });
    expect(screen.getByRole("dialog").getAttribute("aria-label")).toContain("ksia");
  });
});

/**
 * O duplo clique é do card, não dos controles dentro dele: o card do Fluxo não
 * tem botão hoje, e o gesto não pode virar armadilha para o primeiro que vier.
 */
describe("isControlTarget", () => {
  afterEach(cleanup);

  it("reconhece um clique que veio de dentro de um botão", () => {
    const host = document.createElement("div");
    host.innerHTML = "<article><button><span>Sair da rede</span></button></article>";
    document.body.append(host);
    expect(isControlTarget(host.querySelector("span"))).toBe(true);
    expect(isControlTarget(host.querySelector("article"))).toBe(false);
    host.remove();
  });

  it("não trata um alvo sem elemento como controle", () => {
    expect(isControlTarget(null)).toBe(false);
  });
});
