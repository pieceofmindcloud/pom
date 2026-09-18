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
    manage_url: "https://pieceofmind.cloud/enterprise",
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
      manage_url: "https://pieceofmind.cloud/enterprise",
      plugin_version: null,
      lease_expires_at: null,
      available_update: null,
    },
    refetch: vi.fn(),
  },
  pomUpdate: {
    data: { core_update: null as null | { release_id: string; version: string; changelog: string | null } },
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
  it("mostra a licença provisória do POM e dependências dos dois ecossistemas", () => {
    mount();

    expect(screen.getByRole("heading", { name: "Sobre" })).toBeTruthy();
    expect(screen.getByText("Licença provisória (exemplo)")).toBeTruthy();
    expect(screen.getByText("AGPL-3.0 + cláusula de marca registrada - sujeita à definição final")).toBeTruthy();
    expect(screen.getByText("@tanstack/react-query")).toBeTruthy();
    expect(screen.getByText("tokio")).toBeTruthy();
    expect(screen.getAllByText("Frontend").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Rust").length).toBeGreaterThan(0);
    expect(screen.getByRole("link", { name: /@tanstack\/react-query/i }).getAttribute("href")).toBe(
      "https://github.com/TanStack/query",
    );
  });

  it("filtra a lista por nome sem esconder o aviso de licença do POM", () => {
    mount();

    fireEvent.change(screen.getByRole("searchbox", { name: "Buscar dependências" }), {
      target: { value: "tokio" },
    });

    expect(screen.getByText("tokio")).toBeTruthy();
    expect(screen.queryByText("@tanstack/react-query")).toBeNull();
    expect(screen.getByText("Licença provisória (exemplo)")).toBeTruthy();
  });

  it("permite verificar atualizações do pom sob demanda, separado dos plugins", async () => {
    mocks.enterpriseStatus.data = { ...inactiveEnterpriseStatus(), active: true, state: "active" };
    mocks.post.mockResolvedValue({ core_update: null });
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Verificar atualizações" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/pom-update/check"));
    expect(mocks.pomUpdate.refetch).toHaveBeenCalled();
    expect(mocks.post).not.toHaveBeenCalledWith(expect.stringContaining("/api/ui/plugins"));
  });

  it("mostra e aplica a atualização do pom nesta própria tela, sem redirecionar", async () => {
    mocks.enterpriseStatus.data = { ...inactiveEnterpriseStatus(), active: true, state: "active" };
    mocks.pomUpdate.data = {
      core_update: { release_id: "pom_core-0.2.0-linux-x86_64", version: "0.2.0", changelog: null },
    };
    mocks.post.mockResolvedValue({ restarting: true });
    mount();

    expect(screen.getByText("Atualização do POM disponível: 0.2.0")).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Abrir Enterprise" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Instalar e reiniciar" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/pom-update/apply",
      { release_id: "pom_core-0.2.0-linux-x86_64" },
    ));
  });
});
