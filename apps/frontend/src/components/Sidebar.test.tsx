// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import React from "react";
import { MemoryRouter, useLocation } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";
import { ChatSessionProvider, useChatSessionController } from "../lib/chatStore";
import { fakeNode } from "../test/fixtures";
import type { NodeInfo } from "../lib/types";

let sidebarNodeData: NodeInfo | null = null;

vi.mock("../lib/queries", () => ({
  useNode: () => ({ data: sidebarNodeData }),
}));

vi.mock("../lib/networkChatUnread", () => ({
  useNetworkChatUnread: () => ({ unreadCount: 2 }),
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

function RouteProbe() {
  return <output data-testid="route">{useLocation().pathname}</output>;
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
        initialEntries={["/models/slice"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <Sidebar />
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Minimizar menu" }));

    expect(screen.getByRole("complementary").className).toContain("w-[68px]");
    // O menu rola por dentro mesmo recolhido; os submenus são `fixed`, então não são cortados.
    const scroller = screen.getByRole("navigation").querySelector(".overflow-y-auto") as HTMLElement;
    expect(scroller.className).toContain("overscroll-y-contain");
    expect(scroller.className).toContain("overflow-x-hidden");
    expect(screen.getByRole("button", { name: "Modelos" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Fatiar modelos" })).toBeTruthy();
    expect(window.localStorage.getItem("pom.sidebar.collapsed")).toBe("true");
  });

  it("opens the minimized submenu by click and navigates to the chosen destination", () => {
    render(
      <MemoryRouter initialEntries={["/"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR">
          <Sidebar />
          <RouteProbe />
        </LocaleProvider>
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Minimizar menu" }));
    const models = screen.getByRole("button", { name: "Modelos" });
    fireEvent.click(models);
    expect(models.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(screen.getByRole("link", { name: "Encontrar Modelos HF" }));
    expect(screen.getByTestId("route").textContent).toBe("/models/remote");
  });

  it("keeps the logo header at 64px, same as the toolbar", () => {
    render(
      <MemoryRouter initialEntries={["/"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR"><Sidebar /></LocaleProvider>
      </MemoryRouter>,
    );
    expect(screen.getByRole("complementary").firstElementChild?.className).toContain("h-16");
  });

  it("reopens by clicking the logo when the menu is minimized", () => {
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

  it("does not minimize when hovering the logo", () => {
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
   * O menu segue a mesma tabela que barra a rota (`lib/access.ts`). As telas
   * Enterprise são a exceção de descoberta: o item permanece visível sem
   * licença e a tela explica como ativá-lo.
   */
  it("hides from the node profile the entries it cannot open and keeps Enterprise", () => {
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
    expect(screen.getByText("Logs")).toBeTruthy();
    // O que ninguém restringiu continua no lugar.
    expect(screen.getByText("Painel")).toBeTruthy();
    expect(screen.getByText("Modelos")).toBeTruthy();
    expect(screen.getByText("Configurações")).toBeTruthy();
  });

  it("keeps the whole menu for the administrator", () => {
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
    expect(screen.getByTestId("network-chat-unread").textContent).toBe("2");
  });

  it("hides the TLS indicator from the flags", () => {
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

    // O seletor de idioma é uma bandeira com submenu; o TLS fica no mesmo grupo.
    const controls = screen.getByRole("button", { name: /^(Idioma|Language):/ }).parentElement as HTMLElement;
    expect(within(controls).queryByTestId("tls-indicator")).toBeNull();
  });

  it("turns the infer menu into an actionable history and leaves docs/config in the footer", () => {
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
    expect(screen.getByRole("button", { name: "Rede" })).toBeTruthy();
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

  /**
   * O histórico é o menu principal do `infer` e é visto de toda rota. Escolher
   * uma conversa das configurações ou da documentação trocava a conversa ativa
   * e deixava a tela onde estava - o menu parecia ter parado de responder.
   */
  it("goes to the chat when a conversation is chosen from another route", () => {
    window.localStorage.setItem("pom.chat.sessions", JSON.stringify([
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
        initialEntries={["/config/network"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="infer">
            <ChatSessionProvider>
              <Sidebar />
              <RouteProbe />
            </ChatSessionProvider>
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByTestId("route").textContent).toBe("/config/network");

    fireEvent.click(screen.getAllByRole("button", { name: /Conversa antiga/ })[0]);
    expect(screen.getByTestId("route").textContent).toBe("/chat");
  });

  it("goes to the chat when a new conversation is started from the documentation", () => {
    render(
      <MemoryRouter
        initialEntries={["/documentation"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="infer">
            <ChatSessionProvider>
              <Sidebar />
              <RouteProbe />
            </ChatSessionProvider>
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Conversa nova" }));
    expect(screen.getByTestId("route").textContent).toBe("/chat");
  });
});
