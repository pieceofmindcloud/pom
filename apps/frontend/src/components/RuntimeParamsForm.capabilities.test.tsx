// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { CapabilitySwitch } from "./RuntimeParamsForm";

afterEach(cleanup);

function mount(
  state: "supported" | "unsupported" | "unknown",
  alwaysVisible = false,
  warning?: string,
) {
  const onChange = vi.fn();
  render(
    <LocaleProvider initialLocale="pt-BR">
      <CapabilitySwitch
        capability={{
          state,
          source: "metadados GGUF reais",
          reason: state === "unknown" ? "template ausente" : "resultado apurado",
          warning,
        }}
        alwaysVisible={alwaysVisible}
        checked={false}
        onChange={onChange}
        label="Opção condicional"
      />
    </LocaleProvider>,
  );
  return onChange;
}

describe("CapabilitySwitch", () => {
  it("oculta uma opção comprovadamente não suportada", () => {
    mount("unsupported");
    expect(screen.queryByRole("checkbox", { name: "Opção condicional" })).toBeNull();
  });

  it("mantém desconhecida visível, acionável e com aviso de risco", () => {
    mount("unknown");
    const checkbox = screen.getByRole("checkbox", { name: "Opção condicional" });
    expect(checkbox.getAttribute("aria-checked")).toBe("false");
    expect((checkbox as HTMLButtonElement).disabled).toBe(false);
    expect(screen.getByRole("alert").textContent).toContain("template ausente");
    expect(screen.getByRole("alert").textContent).toContain("por conta e risco");
    fireEvent.click(checkbox);
  });

  it("permite a opção suportada e preserva no_embeddings visível", () => {
    const onChange = mount("supported", true);
    fireEvent.click(screen.getByRole("checkbox", { name: "Opção condicional" }));
    expect(onChange).toHaveBeenCalledWith(true);
  });

  it("mantém acionável uma limitação do runtime e mostra o aviso", () => {
    const onChange = mount("supported", false, "peer sem protocolo de lote");
    const checkbox = screen.getByRole("checkbox", { name: "Opção condicional" });
    expect((checkbox as HTMLButtonElement).disabled).toBe(false);
    expect(screen.getByRole("alert").textContent).toContain("peer sem protocolo");
    fireEvent.click(checkbox);
    expect(onChange).toHaveBeenCalledWith(true);
  });

  it("explica a limitação topológica sem atribuí-la ao runtime", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <CapabilitySwitch
          capability={{
            state: "supported",
            source: "topologia do plano",
            reason: "controle visível, peer intermediário precisa exportar residual",
          }}
          checked={false}
          onChange={vi.fn()}
          label="Desativar embeddings"
        />
      </LocaleProvider>,
    );
    expect(screen.getByRole("alert").textContent).toContain("topologia limita");
    expect(screen.getByRole("alert").textContent).not.toContain("runtime");
  });
});
