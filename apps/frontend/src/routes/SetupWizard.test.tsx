// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { MOCK_CAPABILITIES, MOCK_HARDWARE, MOCK_MODELS, MOCK_NODES, MOCK_PUBLIC_NETWORKS } from "./setupWizardMocks";
import { SetupWizard } from "./SetupWizard";

function mount() {
  return render(
    <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale="pt-BR">
        <SetupWizard />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

afterEach(() => cleanup());

describe("dados do wizard", () => {
  it("mantém os mocks separados e com o formato que o backend poderá fornecer", () => {
    expect(MOCK_HARDWARE.gpu.vramGb).toBe(16);
    expect(MOCK_NODES).toHaveLength(2);
    expect(MOCK_MODELS).toHaveLength(3);
    expect(MOCK_PUBLIC_NETWORKS).toHaveLength(3);
    expect(MOCK_CAPABILITIES).toContain("inference.chat");
    expect(MOCK_CAPABILITIES).toContain("inference.api");
  });
});

describe("SetupWizard", () => {
  async function settle() {
    await new Promise((resolve) => window.setTimeout(resolve, 220));
  }

  it("leva o Quick Start até o resumo e permite voltar", async () => {
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Começar" }));
    await settle();
    expect(screen.getByRole("heading", { name: "Hardware encontrado" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /Usar este computador/i }));
    await settle();
    expect(screen.getByRole("heading", { name: "Vamos criar sua rede POM" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Continuar" }));
    await settle();
    fireEvent.click(screen.getByRole("button", { name: /Baixar e usar/i }));
    fireEvent.click(screen.getByRole("button", { name: "Continuar" }));
    await settle();
    fireEvent.click(screen.getByRole("button", { name: "Continuar" }));
    await settle();

    expect(screen.getByRole("heading", { name: "Verificando seu POM" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Continuar" }));
    await settle();
    expect(screen.getByRole("heading", { name: "Tudo pronto." })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Voltar" }));
    await settle();
    expect(screen.getByRole("heading", { name: "Verificando seu POM" })).toBeTruthy();
  });

  it.each([
    "Usar minhas próprias máquinas",
    "Criar uma rede para minha empresa",
    "Contribuir com uma rede",
    "Usar uma rede de IA",
    "Configuração avançada",
  ])("conclui o caminho %s", async (intentLabel) => {
    mount();
    fireEvent.click(screen.getByRole("button", { name: intentLabel }));
    await settle();

    for (let step = 0; step < 12 && !screen.queryByRole("heading", { name: "Tudo pronto." }); step += 1) {
      const next = screen.queryByRole("button", { name: "Continuar" });
      if (next) {
        fireEvent.click(next);
        await settle();
        continue;
      }
      const action = screen.queryByRole("button", { name: /Baixar e usar|Usar este computador|Conectar|Começar a usar o POM/i });
      if (action) {
        fireEvent.click(action);
        await settle();
        continue;
      }
      break;
    }

    expect(screen.getByRole("heading", { name: "Tudo pronto." })).toBeTruthy();
  });
});
