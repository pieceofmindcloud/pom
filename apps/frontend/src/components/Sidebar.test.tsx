// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";
import { ChatSessionProvider, useChatSessionController } from "../lib/chatStore";
import { fakeNode } from "../test/fixtures";
import type { NodeInfo } from "../lib/types";

let sidebarNodeData: NodeInfo | null = null;

vi.mock("../lib/queries", () => ({
  useNode: () => ({ data: sidebarNodeData }),
}));

vi.mock("../lib/theme", () => ({
  useTheme: () => ({ theme: "dark", toggle: vi.fn() }),
}));

vi.mock("./Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));

import { Sidebar } from "./Sidebar";

function ChatSessionProbe() {
  const { session } = useChatSessionController();
  return <output data-testid="active-chat-session">{session.title}</output>;
}

describe("Sidebar", () => {
  afterEach(() => {
    cleanup();
    sidebarNodeData = null;
    vi.useRealTimers();
    window.localStorage.clear();
  });

  it("collapses to icon buttons and exposes child destinations in a hover flyout", () => {
    render(
      <MemoryRouter
        initialEntries={["/modelos/fatiar"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
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
      <MemoryRouter
        initialEntries={["/"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
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

  it("não minimiza ao passar o mouse sobre a logo", () => {
    vi.useFakeTimers();
    render(
      <MemoryRouter
        initialEntries={["/"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <Sidebar />
        </LocaleProvider>
      </MemoryRouter>,
    );

    const logo = screen.getByRole("button", { name: "Minimizar menu" });
    fireEvent.mouseEnter(logo);

    expect(screen.getByRole("button", { name: "Minimizar menu" })).toBeTruthy();
    act(() => vi.advanceTimersByTime(500));
    expect(screen.getByRole("button", { name: "Minimizar menu" })).toBeTruthy();
  });

  /**
   * O menu segue a mesma tabela que barra a rota (`lib/access.ts`). Oferecer no
   * menu o que a rota recusa manda o operador bater numa porta fechada.
   */
  it("esconde do perfil do nó as entradas que ele não abre", () => {
    render(
      <MemoryRouter
        initialEntries={["/"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
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
      <MemoryRouter
        initialEntries={["/"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="admin">
            <Sidebar />
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Implantações" }));
    expect(screen.getByText("Implantar na rede")).toBeTruthy();
    expect(screen.getByText("Histórico")).toBeTruthy();
    expect(screen.getByText("Logs")).toBeTruthy();
  });

  it("mostra o estado TLS junto às bandeiras e sem texto visível", () => {
    sidebarNodeData = fakeNode({ tls_required: true });
    render(
      <MemoryRouter
        initialEntries={["/"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <Sidebar />
        </LocaleProvider>
      </MemoryRouter>,
    );

    const controls = screen.getByRole("group", { name: "Idioma" });
    const indicator = within(controls).getByTestId("tls-indicator");
    expect(indicator.getAttribute("data-tls-state")).toBe("on");
    expect(indicator.textContent).toBe("");
  });

  it("transforma o menu do infer em histórico acionável e deixa docs/config no rodapé", () => {
    window.localStorage.setItem("pom.chat.sessions", JSON.stringify([
      {
        id: "active",
        title: "Conversa ativa",
        model: "qwen",
        messages: [],
        updatedAt: "2026-09-07T10:00:00.000Z",
      },
      {
        id: "older",
        title: "Conversa antiga",
        model: "qwen",
        messages: [],
        updatedAt: "2026-09-06T10:00:00.000Z",
      },
    ]));

    render(
      <MemoryRouter
        initialEntries={["/chat"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="infer">
            <ChatSessionProvider>
              <Sidebar />
              <ChatSessionProbe />
            </ChatSessionProvider>
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByRole("button", { name: "Conversa nova" })).toBeTruthy();
    const activeButton = screen.getAllByRole("button", { name: /Conversa ativa/ })
      .find((button) => button.getAttribute("aria-pressed") === "true");
    expect(activeButton).toBeTruthy();
    expect(screen.getByRole("link", { name: "Rede" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Documentação" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Configurações" })).toBeTruthy();
    expect(screen.queryByText("Painel")).toBeNull();
    expect(screen.getByTestId("chat-session-list").className).toContain("overflow-y-auto");

    const olderChoice = screen.getAllByRole("button", { name: /Conversa antiga/ })
      .find((button) => button.getAttribute("aria-pressed") === "false");
    expect(olderChoice).toBeTruthy();
    fireEvent.click(olderChoice!);
    expect(screen.getByTestId("active-chat-session").textContent).toBe("Conversa antiga");
    const olderButton = screen.getAllByRole("button", { name: /Conversa antiga/ })
      .find((button) => button.getAttribute("aria-pressed") === "true");
    expect(olderButton).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Conversa nova" }));
    expect(screen.getByTestId("active-chat-session").textContent).toBe("Conversa nova");
  });
});
