// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { CapabilitySwitch } from "./RuntimeParamsForm";

afterEach(cleanup);

function mount(
  state: "supported" | "unsupported" | "unknown",
  alwaysVisible = false,
) {
  const onChange = vi.fn();
  render(
    <LocaleProvider initialLocale="pt-BR">
      <CapabilitySwitch
        capability={{
          state,
          source: "metadados GGUF reais",
          reason: state === "unknown" ? "template ausente" : "resultado apurado",
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

  it("mantém desconhecida visível, desabilitada e com motivo", () => {
    mount("unknown");
    const checkbox = screen.getByRole("checkbox", { name: "Opção condicional" });
    expect(checkbox.getAttribute("aria-checked")).toBe("false");
    expect((checkbox as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByRole("alert").textContent).toContain("template ausente");
    fireEvent.click(checkbox);
    expect(checkbox.getAttribute("aria-checked")).toBe("false");
  });

  it("permite a opção suportada e preserva no_embeddings visível", () => {
    const onChange = mount("supported", true);
    fireEvent.click(screen.getByRole("checkbox", { name: "Opção condicional" }));
    expect(onChange).toHaveBeenCalledWith(true);
  });
});
