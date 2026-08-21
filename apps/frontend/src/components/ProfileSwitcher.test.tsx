// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider, useProfile } from "../lib/profile";
import { ProfileSwitcher } from "./ProfileSwitcher";

/** Mostra na tela o perfil ativo, para o teste ler o efeito do clique. */
function PerfilAtivo() {
  const { profile } = useProfile();
  return <span data-testid="ativo">{profile}</span>;
}

function montar() {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <ProfileProvider>
        <ProfileSwitcher />
        <PerfilAtivo />
      </ProfileProvider>
    </LocaleProvider>,
  );
}

describe("ProfileSwitcher", () => {
  afterEach(() => {
    cleanup();
    window.localStorage.clear();
  });

  it("começa em administrador e marca o botão ativo", () => {
    montar();
    expect(screen.getByTestId("ativo").textContent).toBe("admin");
    expect(screen.getByRole("button", { name: "Administrador" }).getAttribute("aria-pressed")).toBe(
      "true",
    );
    expect(screen.getByRole("button", { name: "Nó" }).getAttribute("aria-pressed")).toBe("false");
  });

  it("troca o perfil ativo e lembra a escolha", () => {
    montar();
    fireEvent.click(screen.getByRole("button", { name: "Nó" }));

    expect(screen.getByTestId("ativo").textContent).toBe("node");
    expect(screen.getByRole("button", { name: "Nó" }).getAttribute("aria-pressed")).toBe("true");
    expect(window.localStorage.getItem("pom.profile")).toBe("node");
  });

  /**
   * O seletor é andaime, e a escolha que ele grava é a mesma que a interface
   * lê no próximo carregamento. Um valor estranho no armazenamento - de uma
   * versão anterior, ou digitado à mão - não pode deixar a tela num perfil que
   * não existe.
   */
  it("ignora um perfil desconhecido guardado no navegador", () => {
    window.localStorage.setItem("pom.profile", "root");
    montar();
    expect(screen.getByTestId("ativo").textContent).toBe("admin");
  });
});
