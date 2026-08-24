// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";

vi.mock("../lib/queries", () => ({
  useNode: () => ({ data: null }),
}));

vi.mock("../lib/theme", () => ({
  useTheme: () => ({ theme: "dark", toggle: vi.fn() }),
}));

vi.mock("./Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));

import { Sidebar } from "./Sidebar";

describe("Sidebar", () => {
  afterEach(() => {
    cleanup();
    vi.useRealTimers();
    window.localStorage.clear();
  });

  it("collapses to icon buttons and exposes child destinations in a hover flyout", () => {
    render(
      <MemoryRouter initialEntries={["/modelos/fatiar"]}>
        <LocaleProvider initialLocale="pt-BR">
          <Sidebar />
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Minimizar menu" }));

    expect(screen.getByRole("complementary").className).toContain("w-[68px]");
    expect(screen.getByRole("button", { name: "Modelos" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Fatiar modelos" })).toBeTruthy();
    expect(window.localStorage.getItem("pom.sidebar.collapsed")).toBe("true");
  });

  it("reabre pelo clique na logo quando o menu está minimizado", () => {
    render(
      <MemoryRouter initialEntries={["/"]}>
        <LocaleProvider initialLocale="pt-BR">
          <Sidebar />
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Minimizar menu" }));
    const logo = screen.getByRole("button", { name: "Expandir menu" });

    expect(logo.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(logo);
    expect(screen.getByRole("button", { name: "Minimizar menu" }).getAttribute("aria-expanded")).toBe(
      "true",
    );
  });

  it("fecha pelo hover da logo somente depois de um pequeno atraso", () => {
    vi.useFakeTimers();
    render(
      <MemoryRouter initialEntries={["/"]}>
        <LocaleProvider initialLocale="pt-BR">
          <Sidebar />
        </LocaleProvider>
      </MemoryRouter>,
    );

    const logo = screen.getByRole("button", { name: "Minimizar menu" });
    fireEvent.mouseEnter(logo);

    expect(screen.getByRole("button", { name: "Minimizar menu" })).toBeTruthy();
    act(() => vi.advanceTimersByTime(219));
    expect(screen.getByRole("button", { name: "Minimizar menu" })).toBeTruthy();
    act(() => vi.advanceTimersByTime(1));
    expect(screen.getByRole("button", { name: "Expandir menu" })).toBeTruthy();
  });

  /**
   * O menu segue a mesma tabela que barra a rota (`lib/access.ts`). Oferecer no
   * menu o que a rota recusa manda o operador bater numa porta fechada.
   */
  it("esconde do perfil do nó as entradas que ele não abre", () => {
    render(
      <MemoryRouter initialEntries={["/"]}>
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="node">
            <Sidebar />
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.queryByText("Implantar na rede")).toBeNull();
    expect(screen.queryByText("Implantações")).toBeNull();
    expect(screen.queryByText("Logs")).toBeNull();
    // O que ninguém restringiu continua no lugar.
    expect(screen.getByText("Painel")).toBeTruthy();
    expect(screen.getByText("Modelos")).toBeTruthy();
    expect(screen.getByText("Configurações")).toBeTruthy();
  });

  it("mantém o menu inteiro para o administrador", () => {
    render(
      <MemoryRouter initialEntries={["/"]}>
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="admin">
            <Sidebar />
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("Implantar na rede")).toBeTruthy();
    expect(screen.getByText("Implantações")).toBeTruthy();
    expect(screen.getByText("Logs")).toBeTruthy();
  });
});
