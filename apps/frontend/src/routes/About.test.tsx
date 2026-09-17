// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { About } from "./About";

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  refetch: vi.fn(),
  status: {
    data: {
      active: false,
      state: "needs_activation",
      features: { request_logs: false, statistics: false },
      manage_url: "https://pieceofmind.cloud/enterprise",
      plugin_version: null,
      lease_expires_at: null,
      core_update: null,
    },
    refetch: vi.fn(),
  },
}));

vi.mock("../lib/api", () => ({ api: { post: mocks.post } }));
vi.mock("../lib/queries", () => ({ useEnterpriseStatus: () => mocks.status }));

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
  mocks.status.refetch.mockReset();
  mocks.status.data = {
    active: false,
    state: "needs_activation",
    features: { request_logs: false, statistics: false },
    manage_url: "https://pieceofmind.cloud/enterprise",
    plugin_version: null,
    lease_expires_at: null,
    core_update: null,
  };
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

  it("mostra o estado Enterprise e permite verificar atualizações sob demanda", async () => {
    mocks.status.data = {
      ...mocks.status.data,
      active: true,
      state: "active",
    };
    mocks.post.mockResolvedValue({ ...mocks.status.data });
    mount();

    expect(screen.getByText("Licença Enterprise")).toBeTruthy();
    expect(screen.getAllByText("Ativa")).toHaveLength(2);
    fireEvent.click(screen.getByRole("button", { name: "Verificar atualizações" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/enterprise/pom-update/check",
    ));
    expect(mocks.status.refetch).toHaveBeenCalled();
  });

  it("encaminha uma atualização detectada para a tela Enterprise", () => {
    mocks.status.data = {
      ...mocks.status.data,
      active: true,
      state: "active",
      core_update: { release_id: "pom_core-0.2.0-linux-x86_64", version: "0.2.0", changelog: null },
    };
    mount();

    expect(screen.getByText("Atualização do POM disponível: 0.2.0")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Abrir Enterprise" }).getAttribute("href")).toBe("/enterprise");
  });
});
