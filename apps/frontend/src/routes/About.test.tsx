// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { About } from "./About";

function inactiveEnterpriseStatus() {
  return {
    active: false,
    state: "needs_activation",
    features: [] as string[],
    manage_url: "https://pieceofmind.cloud/plugins/",
    plugin_version: null,
    lease_expires_at: null,
    available_update: null,
  };
}

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  enterpriseStatus: {
    data: {
      active: false,
      state: "needs_activation",
      features: [] as string[],
      manage_url: "https://pieceofmind.cloud/plugins/",
      plugin_version: null,
      lease_expires_at: null,
      available_update: null,
    },
    refetch: vi.fn(),
  },
  pomUpdate: {
    data: { core_update: null } as { core_update: null | Record<string, unknown>; current_version?: string },
    refetch: vi.fn(),
  },
}));

vi.mock("../lib/api", () => ({ api: { post: mocks.post } }));
vi.mock("../lib/queries", () => ({
  useEnterpriseStatus: () => mocks.enterpriseStatus,
  usePomCoreUpdateStatus: () => mocks.pomUpdate,
}));

function mount() {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <About />
      </MemoryRouter>
    </LocaleProvider>,
  );
}

afterEach(() => {
  cleanup();
  mocks.post.mockReset();
  mocks.enterpriseStatus.refetch.mockReset();
  mocks.pomUpdate.refetch.mockReset();
  mocks.enterpriseStatus.data = inactiveEnterpriseStatus();
  mocks.pomUpdate.data = { core_update: null };
  localStorage.clear();
});

describe("About", () => {
  it("shows the MIT license, the LGPD notice and the project links", () => {
    mount();

    expect(screen.getByRole("heading", { name: "Sobre" })).toBeTruthy();
    expect(screen.getAllByText("MIT").length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText("Aviso de privacidade e segurança (LGPD)")).toBeTruthy();
    expect(screen.getByText(/não pode garantir segurança ou privacidade em redes públicas/i)).toBeTruthy();
    expect(screen.getByRole("link", { name: /^MIT/ }).getAttribute("href")).toBe("https://opensource.org/license/mit");
    expect(screen.getByRole("link", { name: "Arquivo LICENSE do repositório" }).getAttribute("href")).toBe(
      "https://github.com/pieceofmindcloud/pom/blob/main/LICENSE",
    );
    expect(screen.getByRole("link", { name: "Site do POM" }).getAttribute("href")).toBe("https://pieceofmind.cloud");
    expect(screen.getByRole("link", { name: "Contato do POM" }).getAttribute("href")).toBe("https://pieceofmind.cloud/contato/");
    expect(screen.getByRole("link", { name: "Repositório oficial do POM no GitHub" }).getAttribute("href")).toBe("https://github.com/pieceofmindcloud/pom");
    expect(screen.getByRole("link", { name: "POM Plugin Base" }).getAttribute("href")).toBe("https://github.com/pieceofmindcloud/pom-plugin-base");
    expect(screen.getByText("@tanstack/react-query")).toBeTruthy();
    expect(screen.getByText("tokio")).toBeTruthy();
    expect(screen.getAllByText("Frontend").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Rust").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Nativa").length).toBeGreaterThan(0);
    expect(screen.getByText("llama.cpp / ggml")).toBeTruthy();
    expect(screen.getByRole("link", { name: /@tanstack\/react-query/i }).getAttribute("href")).toBe(
      "https://github.com/TanStack/query",
    );
  });

  it("filters the list by name without hiding the POM license notice", () => {
    mount();

    fireEvent.change(screen.getByRole("searchbox", { name: "Buscar dependências" }), {
      target: { value: "tokio" },
    });

    expect(screen.getByText("tokio")).toBeTruthy();
    expect(screen.queryByText("@tanstack/react-query")).toBeNull();
    expect(screen.getByText("Aviso de privacidade e segurança (LGPD)")).toBeTruthy();
  });

  it("checks the pom releases on demand, without needing any plugin license", async () => {
    mocks.post.mockResolvedValue({ core_update: null, current_version: "v0.5.6" });
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Verificar atualizações" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/pom-update/check"));
    expect(mocks.pomUpdate.refetch).toHaveBeenCalled();
    expect(await screen.findByText("O POM está atualizado.")).toBeTruthy();
    expect(mocks.post).not.toHaveBeenCalledWith(expect.stringContaining("/api/ui/plugins"));
  });

  it("shows and applies the pom update found on GitHub on this very screen", async () => {
    mocks.pomUpdate.data = {
      current_version: "v0.5.5",
      core_update: {
        release_id: "v0.5.6", version: "0.5.6", changelog: "Notes", published_at: null, source: "github",
        asset: "pom-v0.5.6-x86_64.AppImage", size: 10, installable: true, blocked: null,
      },
    };
    mocks.post.mockResolvedValue({ restarting: true });
    mount();

    expect(screen.getByText("Atualização do POM disponível: v0.5.6")).toBeTruthy();
    expect(screen.getByText("v0.5.5")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Instalar e reiniciar" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/pom-update/apply",
      { release_id: "v0.5.6" },
    ));
    expect(await screen.findByText(/O POM está reiniciando/)).toBeTruthy();
  });

  it("says why a release the site lists cannot be installed here", () => {
    mocks.pomUpdate.data = {
      core_update: {
        release_id: "v0.5.6", version: "0.5.6", changelog: null, published_at: null, source: "site",
        asset: "pom-v0.5.6-x86_64.AppImage", size: 10, installable: false, blocked: "invitation_required",
      },
    };
    mount();
    expect(screen.queryByRole("button", { name: "Instalar e reiniciar" })).toBeNull();
    expect(screen.getByText(/download ainda exige convite/)).toBeTruthy();
  });
});
