// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { Enterprise } from "./Enterprise";

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
    },
    refetch: vi.fn(),
  },
}));

vi.mock("../lib/api", () => ({ api: { post: mocks.post } }));
vi.mock("../lib/queries", () => ({ useEnterpriseStatus: () => mocks.status }));

function renderPage() {
  return render(
    <MemoryRouter>
      <LocaleProvider initialLocale="pt-BR">
        <Enterprise />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("Enterprise", () => {
  afterEach(() => {
    cleanup();
    mocks.post.mockReset();
    mocks.status.refetch.mockReset();
    localStorage.clear();
  });

  it("envia a license key uma vez, limpa o campo e nunca a grava no storage", async () => {
    mocks.post.mockResolvedValue({});
    renderPage();

    const input = screen.getByLabelText("Chave de licença Enterprise");
    fireEvent.change(input, { target: { value: "PM-secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Ativar licença" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/enterprise/activate",
      { license_key: "PM-secret" },
    ));
    expect((input as HTMLInputElement).value).toBe("");
    expect(localStorage.getItem("PM-secret")).toBeNull();
    expect(mocks.status.refetch).toHaveBeenCalled();
  });

  it("explica uma recusa sem exibir a chave na mensagem", async () => {
    mocks.post.mockRejectedValue(new Error("licença inválida"));
    renderPage();

    fireEvent.change(screen.getByLabelText("Chave de licença Enterprise"), {
      target: { value: "PM-invalid" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Ativar licença" }));

    expect(await screen.findByText("Não foi possível ativar a licença.")).toBeTruthy();
    expect(screen.queryByText("PM-invalid")).toBeNull();
  });
});
