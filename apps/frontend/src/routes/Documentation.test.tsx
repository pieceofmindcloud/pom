// @vitest-environment jsdom
/**
 * A rota Documentação é auto-contida: o conteúdo viaja embutido no bundle
 * (`scripts/build-docs.mjs` gera `src/docs/` no build, os guias vivem em
 * `src/guides/`), então o que se testa aqui é o contrato entre a tela e esse
 * conjunto - duas abas para dois públicos, a árvore que navega o repositório,
 * a busca, e o endereço (`?tab=`, `?guide=`, `?doc=`).
 *
 * O jsdom não calcula layout; os testes que valem a pena aqui são de
 * comportamento, não de posição. As afirmações sobre a rolagem são estruturais
 * (quem é o container que rola), porque é a estrutura que estava errada.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { api } from "../lib/api";
import { setPluginUi, type ActivePluginUi } from "../lib/pluginUi";
import { Documentation } from "./Documentation";

const documentedPlugin: ActivePluginUi = {
  code: "docs-plugin",
  version: "1.0.0",
  manifest: {
    schema: "pom-plugin-ui/v1",
    plugin_code: "docs-plugin",
    menu: [],
    routes: [{ path: "/", screen: "home", profiles: ["admin"] }],
    screens: { home: { module: "ui/home.js" } },
    documentation: {
      guides: [{
        id: "start",
        title: { en: "Plugin guide" },
        asset: "docs/guides/start.md",
      }],
      reference: [{
        id: "reference",
        title: { en: "Plugin reference" },
        asset: "docs/reference/reference.md",
      }],
    },
    assets: ["ui/home.js", "docs/guides/start.md", "docs/reference/reference.md"],
  },
};

function renderDoc(initialEntry = "/documentation") {
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
  afterEach(() => setPluginUi([]));

  it("opens on the guides, not on the agent markdown", () => {
    renderDoc();
    // O primeiro guia é o "o que é esta plataforma"; o markdown da raiz não
    // pode ser a primeira coisa que um humano vê.
    expect(screen.getByRole("heading", { name: "What this platform is", level: 2 })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "AGENTS.md - raiz" })).toBeNull();
  });

  it("a guide takes the reader to the reference through the deep-dive link", () => {
    renderDoc();
    fireEvent.click(screen.getByRole("button", { name: /Discovery, advertised addresses/ }));
    // O leitor imprime o assunto no próprio cabeçalho e o markdown entra sem o
    // H1 dele, que só repetiria o mesmo em outra redação.
    expect(screen.getByRole("heading", { name: "P2P network and discovery", level: 2 })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 1 })).toBeNull();
    expect(screen.getByRole("tab", { name: "Reference" }).getAttribute("aria-selected")).toBe(
      "true",
    );
  });

  it("the file list shows neither a file name nor AGENTS.md", () => {
    renderDoc();
    openReference();
    // "backend/" e não "AGENTS.md - crates/node/src/backend/"; "P2P network
    // and discovery" e não "p2p-network.md".
    expect(screen.getByRole("button", { name: "Open backend/" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Open P2P network and discovery" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Open AGENTS\.md/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /\.md$/ })).toBeNull();
  });

  it("the section header opens the overview of that folder", () => {
    renderDoc();
    openReference();
    fireEvent.click(screen.getByRole("button", { name: "docs/" }));
    expect(screen.getByRole("heading", { name: "docs/", level: 2 })).toBeTruthy();
  });

  it("a section can be collapsed even while holding the open document", async () => {
    renderDoc("/documentation?tab=reference&doc=docs/subsystems/inference.md");
    const collapse = screen.getByRole("button", { name: "Collapse Subsystem docs" });
    expect(screen.getByRole("button", { name: "Open Inference path" })).toBeTruthy();

    fireEvent.click(collapse);

    // O defeito antigo: a seção que continha a seleção voltava a abrir sozinha.
    // A saída é animada, então a folha some no fim da animação, não no clique.
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "Open Inference path" })).toBeNull(),
    );
    expect(screen.getByRole("button", { name: "Expand Subsystem docs" })).toBeTruthy();
    // Recolher não muda o que está sendo lido.
    expect(screen.getByRole("heading", { name: "Inference path", level: 2 })).toBeTruthy();
  });

  it("the navigation list scrolls on its own, not the page", () => {
    const { container } = renderDoc();
    openReference();
    // A regressão foi estrutural: sem `min-h-0` o filho flex cresce com o
    // conteúdo e o overflow nunca engata, então a busca corria para fora da
    // tela sem barra de rolagem.
    const scrollers = container.querySelectorAll(".overflow-y-auto");
    expect(scrollers.length).toBe(2);
    expect(scrollers[0].className).toContain("min-h-0");
  });

  it("?doc= and ?guide= are addresses", () => {
    renderDoc("/documentation?tab=reference&doc=docs/subsystems/inference.md");
    expect(screen.getByRole("heading", { name: "Inference path", level: 2 })).toBeTruthy();
    cleanup();

    renderDoc("/documentation?tab=guides&guide=operating");
    expect(screen.getByRole("heading", { name: "Operate and diagnose", level: 2 })).toBeTruthy();
  });

  it("the search filters by label and body; clearing returns to the tree", () => {
    renderDoc();
    openReference();
    const search = screen.getByLabelText("Search documentation");
    fireEvent.change(search, { target: { value: "transfer" } });
    // Com busca ativa a árvore some; "Project root" ainda aparece como *dica*
    // de seção num resultado, então o que tem de sumir é o cabeçalho.
    expect(screen.queryByRole("button", { name: "Project root" })).toBeNull();
    expect(screen.getAllByRole("button", { name: /Open .*transfer/i }).length).toBeGreaterThan(0);

    fireEvent.click(screen.getByLabelText("Clear search"));
    expect(screen.getByRole("button", { name: "Project root" })).toBeTruthy();
  });

  it("a search without results warns instead of breaking", () => {
    renderDoc();
    openReference();
    fireEvent.change(screen.getByLabelText("Search documentation"), {
      target: { value: "zzz-não-existe" },
    });
    expect(screen.getByText("No documentation matches")).toBeTruthy();
  });

  it("displays the guides of an installed plugin and loads the Markdown on demand", async () => {
    setPluginUi([documentedPlugin]);
    vi.spyOn(api, "getText").mockResolvedValue("# Plugin guide\n\nPlugin content");
    renderDoc();

    fireEvent.click(screen.getByRole("button", { name: /Plugin guide docs-plugin/ }));

    await waitFor(() => {
      expect(screen.getByRole("heading", { name: "Plugin guide", level: 2 })).toBeTruthy();
      expect(screen.getByText("Plugin content")).toBeTruthy();
    });
    expect(api.getText).toHaveBeenCalledWith(
      "/api/ui/plugins/docs-plugin/assets/docs/guides/start.md",
      { auth: false },
    );
  });

  describe("mobile navigation", () => {
    it("starts on the full-screen bar, without the reader or the Back button", () => {
      const { container } = renderDoc();
      expect(screen.queryByRole("button", { name: "Back" })).toBeNull();
      expect(container.querySelector("nav")?.className).not.toContain("hidden");
      expect(screen.getByTestId("docs-reader").className).toContain("hidden");
    });

    it("selecting an item opens the reader full screen with Back at the top", () => {
      const { container } = renderDoc();
      fireEvent.click(screen.getByRole("button", { name: /Discovery, advertised addresses/ }));

      expect(screen.getByRole("heading", { name: "P2P network and discovery", level: 2 })).toBeTruthy();
      expect(screen.getByRole("button", { name: "Back" })).toBeTruthy();
      expect(container.querySelector("nav")?.className).toContain("hidden");
      expect(screen.getByTestId("docs-reader").className).not.toContain("hidden");
    });

    it("Back closes the reader without losing the selection", () => {
      const { container } = renderDoc();
      fireEvent.click(screen.getByRole("button", { name: /Discovery, advertised addresses/ }));
      fireEvent.click(screen.getByRole("button", { name: "Back" }));

      expect(screen.queryByRole("button", { name: "Back" })).toBeNull();
      expect(container.querySelector("nav")?.className).not.toContain("hidden");
      expect(screen.getByTestId("docs-reader").className).toContain("hidden");
      // O conteúdo lido continua sendo o mesmo - só a vista mobile mudou.
      expect(screen.getByRole("heading", { name: "P2P network and discovery", level: 2 })).toBeTruthy();
    });

    it("a deep link already arrives as a selection, opening straight in the reader", () => {
      renderDoc("/documentation?tab=reference&doc=docs/subsystems/inference.md");
      expect(screen.getByRole("button", { name: "Back" })).toBeTruthy();
      expect(screen.getByRole("heading", { name: "Inference path", level: 2 })).toBeTruthy();
    });
  });
});
