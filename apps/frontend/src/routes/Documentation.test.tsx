// @vitest-environment jsdom
/**
 * The Documentation route is self-contained: the content travels embedded in the
 * bundle (`scripts/build-docs.mjs` generates `src/docs/` at build, the guides
 * live in `src/guides/`), so what is tested here is the contract between the
 * screen and that set - two tabs for two audiences, the tree that navigates the
 * repository, the search, and the address (`?tab=`, `?guide=`, `?doc=`).
 *
 * jsdom does not compute layout; the tests worth having here are about
 * behaviour, not position. The assertions about scrolling are structural
 * (which container scrolls), because the structure is what was wrong.
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

/** Opens the reference tab, which is where the tree and the search live. */
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
    // The first guide is the "what is this platform" one; the root markdown must
    // not be the first thing a human sees.
    expect(screen.getByRole("heading", { name: "What this platform is", level: 2 })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "AGENTS.md - raiz" })).toBeNull();
  });

  it("a guide takes the reader to the reference through the deep-dive link", () => {
    renderDoc();
    fireEvent.click(screen.getByRole("button", { name: /Discovery, advertised addresses/ }));
    // The reader prints the subject in its own header and the markdown comes in
    // without its H1, which would only repeat the same in other wording.
    expect(screen.getByRole("heading", { name: "P2P network and discovery", level: 2 })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 1 })).toBeNull();
    expect(screen.getByRole("tab", { name: "Reference" }).getAttribute("aria-selected")).toBe(
      "true",
    );
  });

  it("the file list shows neither a file name nor AGENTS.md", () => {
    renderDoc();
    openReference();
    // "backend/" and not "AGENTS.md - crates/node/src/backend/"; "P2P network
    // and discovery" and not "p2p-network.md".
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

    // The old defect: the section holding the selection opened again on its own.
    // The exit is animated, so the leaf disappears at the end of the animation, not on the click.
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "Open Inference path" })).toBeNull(),
    );
    expect(screen.getByRole("button", { name: "Expand Subsystem docs" })).toBeTruthy();
    // Collapsing does not change what is being read.
    expect(screen.getByRole("heading", { name: "Inference path", level: 2 })).toBeTruthy();
  });

  it("the navigation list scrolls on its own, not the page", () => {
    const { container } = renderDoc();
    openReference();
    // The regression was structural: without `min-h-0` the flex child grows with
    // the content and the overflow never engages, so the search ran off the
    // screen with no scrollbar.
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
    // With an active search the tree disappears; "Project root" still shows as a
    // section *hint* in a result, so what has to disappear is the header.
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

  it("the guides tab searches every guide's text and groups the results", () => {
    renderDoc();
    const search = screen.getByLabelText("Search documentation");
    // "Lobby" is only in the body of the Networks and Network Chat screen guides.
    fireEvent.change(search, { target: { value: "lobby" } });
    expect(screen.getByRole("button", { name: /^Network Chat/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^What this platform is/ })).toBeNull();
    expect(screen.getByText("Screens")).toBeTruthy();

    fireEvent.change(search, { target: { value: "zzz-não-existe" } });
    expect(screen.getByText("No documentation matches")).toBeTruthy();
  });

  it("an AGENTS.md with an explanation for people links to it", () => {
    renderDoc("/documentation?tab=reference&doc=crates/node/src/network/AGENTS.md");
    fireEvent.click(screen.getByRole("button", { name: "Read the explanation for people" }));
    expect(screen.getByRole("heading", { name: "Inside the network", level: 2 })).toBeTruthy();
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

  it("a plugin page reads its Portuguese file and resolves its images", async () => {
    setPluginUi([{
      ...documentedPlugin,
      manifest: {
        ...documentedPlugin.manifest,
        assets: [...documentedPlugin.manifest.assets, "docs/guides/start.pt-BR.md", "docs/images/shot.png"],
      },
    }]);
    vi.spyOn(api, "getText").mockResolvedValue("# Guia\n\n![A tela](../images/shot.png)");
    render(
      <MemoryRouter
        initialEntries={["/documentation?tab=guides&guide=plugin:docs-plugin:guide:start"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <Documentation />
        </LocaleProvider>
      </MemoryRouter>,
    );

    await waitFor(() => {
      expect(screen.getByRole("img", { name: "A tela" }).getAttribute("src")).toBe(
        "/api/ui/plugins/docs-plugin/assets/docs/images/shot.png",
      );
    });
    expect(api.getText).toHaveBeenCalledWith(
      "/api/ui/plugins/docs-plugin/assets/docs/guides/start.pt-BR.md",
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
      // The content being read stays the same - only the mobile view changed.
      expect(screen.getByRole("heading", { name: "P2P network and discovery", level: 2 })).toBeTruthy();
    });

    it("a deep link already arrives as a selection, opening straight in the reader", () => {
      renderDoc("/documentation?tab=reference&doc=docs/subsystems/inference.md");
      expect(screen.getByRole("button", { name: "Back" })).toBeTruthy();
      expect(screen.getByRole("heading", { name: "Inference path", level: 2 })).toBeTruthy();
    });
  });
});
