// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { Plugins } from "./Plugins";

function inactivePlugin() {
  return {
    code: "enterprise",
    active: false,
    state: "needs_activation",
    features: [],
    manage_url: "https://pieceofmind.cloud/enterprise",
    plugin_version: null,
    lease_expires_at: null,
    available_update: null,
  };
}

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  plugins: {
    data: [inactivePlugin()],
    isLoading: false,
    isError: false,
    error: null,
    refetch: vi.fn(),
  },
}));

vi.mock("../lib/api", () => ({ api: { post: mocks.post } }));
vi.mock("../lib/queries", () => ({ usePlugins: () => mocks.plugins }));

function renderPage() {
  return render(
    <MemoryRouter>
      <LocaleProvider initialLocale="pt-BR">
        <Plugins />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("Plugins", () => {
  afterEach(() => {
    cleanup();
    mocks.post.mockReset();
    mocks.plugins.data = [inactivePlugin()];
    mocks.plugins.refetch.mockReset();
    localStorage.clear();
  });

  it("envia a license key uma vez, limpa o campo e nunca a grava no storage", async () => {
    mocks.post.mockResolvedValue({ activated: ["enterprise"] });
    renderPage();

    const input = screen.getByLabelText("Chave de licença");
    fireEvent.change(input, { target: { value: "PM-secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Ativar licença" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/activate",
      { license_key: "PM-secret" },
    ));
    expect((input as HTMLInputElement).value).toBe("");
    expect(localStorage.getItem("PM-secret")).toBeNull();
    expect(mocks.plugins.refetch).toHaveBeenCalled();
  });

  it("explica uma recusa sem exibir a chave na mensagem", async () => {
    mocks.post.mockRejectedValue(new Error("licença inválida"));
    renderPage();

    fireEvent.change(screen.getByLabelText("Chave de licença"), {
      target: { value: "PM-invalid" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Ativar licença" }));

    expect(await screen.findByText("Não foi possível ativar a licença.")).toBeTruthy();
    expect(screen.queryByText("PM-invalid")).toBeNull();
  });

  it("lista todo plugin do catálogo, mesmo o que ainda não foi ativado", () => {
    renderPage();

    expect(screen.getByText("Não ativado")).toBeTruthy();
  });

  it("mostra e aplica a atualização de UM plugin, sem tocar o pom_core", async () => {
    mocks.post.mockResolvedValue({});
    mocks.plugins.data = [
      {
        ...inactivePlugin(),
        active: true,
        state: "active",
        features: ["request_logs", "statistics"],
        plugin_version: "0.1.0",
        available_update: { release_id: "ent-2", version: "0.2.0", changelog: null },
      },
    ];
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Instalar atualização" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/enterprise/update/apply",
      { release_id: "ent-2" },
    ));
  });

  it("desativa um plugin sem chamar rota de atualização do pom_core", async () => {
    mocks.post.mockResolvedValue({});
    mocks.plugins.data = [
      {
        ...inactivePlugin(),
        active: true,
        state: "active",
        features: ["request_logs", "statistics"],
        plugin_version: "0.1.0",
      },
    ];
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Desativar" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/enterprise/deactivate"));
    expect(mocks.post).not.toHaveBeenCalledWith(expect.stringContaining("pom-update"));
  });
});
