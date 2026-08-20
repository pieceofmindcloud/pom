// @vitest-environment jsdom
/**
 * A rota Documentação é auto-contida: o conteúdo viaja embutido no bundle
 * (`scripts/build-docs.mjs` gera `src/docs/` no build, os guias vivem em
 * `src/guides/`), então o que se testa aqui é o contrato entre a tela e esse
 * conjunto - duas abas para dois públicos, a árvore que navega o repositório,
 * a busca, e o endereço (`?tab=`, `?guia=`, `?doc=`).
 *
 * O jsdom não calcula layout; os testes que valem a pena aqui são de
 * comportamento, não de posição. As afirmações sobre a rolagem são estruturais
 * (quem é o container que rola), porque é a estrutura que estava errada.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { Documentation } from "./Documentation";

function renderDoc(initialEntry = "/documentacao") {
  return render(
    <MemoryRouter
      initialEntries={[initialEntry]}
      future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
    >
      <LocaleProvider initialLocale="en">
        <Documentation />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

/** Abre a aba de referência, que é onde a árvore e a busca vivem. */
function openReference() {
  fireEvent.click(screen.getByRole("tab", { name: "Reference" }));
}

describe("Documentation", () => {
  beforeEach(() => {
    vi.spyOn(window, "scrollTo").mockImplementation(() => {});
  });

  afterEach(cleanup);
  afterEach(() => vi.restoreAllMocks());

  it("abre nos guias, não no markdown de agente", () => {
    renderDoc();
    // O primeiro guia é o "o que é esta plataforma"; o markdown da raiz não
    // pode ser a primeira coisa que um humano vê.
    expect(screen.getByRole("heading", { name: "What this platform is", level: 2 })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "AGENTS.md - raiz" })).toBeNull();
  });

  it("um guia leva o leitor à referência pelo link de aprofundamento", () => {
    renderDoc();
    fireEvent.click(screen.getByRole("button", { name: /Discovery, advertised addresses/ }));
    // O leitor imprime o assunto no próprio cabeçalho e o markdown entra sem o
    // H1 dele, que só repetiria o mesmo em outra redação.
    expect(screen.getByRole("heading", { name: "Rede P2P e descoberta", level: 2 })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 1 })).toBeNull();
    expect(screen.getByRole("tab", { name: "Reference" }).getAttribute("aria-selected")).toBe(
      "true",
    );
  });

  it("a lista de arquivos não mostra nome de arquivo nem AGENTS.md", () => {
    renderDoc();
    openReference();
    // "backend/" e não "AGENTS.md - crates/node/src/backend/"; "Rede P2P e
    // descoberta" e não "rede-p2p.md".
    expect(screen.getByRole("button", { name: "Open backend/" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Open Rede P2P e descoberta" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Open AGENTS\.md/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /\.md$/ })).toBeNull();
  });

  it("o cabeçalho da seção abre a visão geral daquela pasta", () => {
    renderDoc();
    openReference();
    fireEvent.click(screen.getByRole("button", { name: "docs/" }));
    expect(screen.getByRole("heading", { name: "docs/", level: 2 })).toBeTruthy();
  });

  it("uma seção pode ser recolhida mesmo segurando o documento aberto", async () => {
    renderDoc("/documentacao?tab=referencia&doc=docs/subsystems/inferencia.md");
    const collapse = screen.getByRole("button", { name: "Collapse Subsystem docs" });
    expect(screen.getByRole("button", { name: "Open Caminho de inferência" })).toBeTruthy();

    fireEvent.click(collapse);

    // O defeito antigo: a seção que continha a seleção voltava a abrir sozinha.
    // A saída é animada, então a folha some no fim da animação, não no clique.
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "Open Caminho de inferência" })).toBeNull(),
    );
    expect(screen.getByRole("button", { name: "Expand Subsystem docs" })).toBeTruthy();
    // Recolher não muda o que está sendo lido.
    expect(screen.getByRole("heading", { name: "Caminho de inferência", level: 2 })).toBeTruthy();
  });

  it("a lista de navegação rola sozinha, e não a página", () => {
    const { container } = renderDoc();
    openReference();
    // A regressão foi estrutural: sem `min-h-0` o filho flex cresce com o
    // conteúdo e o overflow nunca engata, então a busca corria para fora da
    // tela sem barra de rolagem.
    const scrollers = container.querySelectorAll(".overflow-y-auto");
    expect(scrollers.length).toBe(2);
    expect(scrollers[0].className).toContain("min-h-0");
  });

  it("?doc= e ?guia= são endereços", () => {
    renderDoc("/documentacao?tab=referencia&doc=docs/subsystems/inferencia.md");
    expect(screen.getByRole("heading", { name: "Caminho de inferência", level: 2 })).toBeTruthy();
    cleanup();

    renderDoc("/documentacao?tab=guias&guia=operar");
    expect(screen.getByRole("heading", { name: "Operate and diagnose", level: 2 })).toBeTruthy();
  });

  it("a busca filtra por rótulo e corpo; limpar volta à árvore", () => {
    renderDoc();
    openReference();
    const search = screen.getByLabelText("Search documentation");
    fireEvent.change(search, { target: { value: "transferência" } });
    // Com busca ativa a árvore some; "Project root" ainda aparece como *dica*
    // de seção num resultado, então o que tem de sumir é o cabeçalho.
    expect(screen.queryByRole("button", { name: "Project root" })).toBeNull();
    expect(screen.getByRole("button", { name: /Open .*ransferência/ })).toBeTruthy();

    fireEvent.click(screen.getByLabelText("Clear search"));
    expect(screen.getByRole("button", { name: "Project root" })).toBeTruthy();
  });

  it("uma busca sem resultado avisa em vez de quebrar", () => {
    renderDoc();
    openReference();
    fireEvent.change(screen.getByLabelText("Search documentation"), {
      target: { value: "zzz-não-existe" },
    });
    expect(screen.getByText("No documentation matches")).toBeTruthy();
  });
});
