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
  // The plugin menu reads its lifecycle here; no plugin is active in these tests.
  usePluginUi: () => ({ data: { plugins: [] } }),
  keys: { pluginUi: ["plugin-ui"] },
}));

// The plugin menu invalidates the plugin list after open/close; no client is mounted here.
vi.mock("@tanstack/react-query", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@tanstack/react-query")>()),
  useQueryClient: () => ({ invalidateQueries: () => Promise.resolve() }),
}));

const lobby = vi.hoisted(() => ({ enabledOnNetwork: true, unread: 3 }));

vi.mock("../lib/networkChatUnread", () => ({
  useNetworkChatUnread: () => ({ unreadCount: 2, snapshot: { lobbyEnabled: lobby.enabledOnNetwork } }),
  // The real hook asks the lobby only when enabled; so does the fake.
  useLobbyUnread: (enabled: boolean) => (enabled ? lobby.unread : 0),
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

/** A conversation somebody wrote in: only those make the history appear. */
const SPOKEN = [{ id: "m1", role: "user", content: "oi", at: "2026-09-06T10:00:00.000Z" }];

describe("Sidebar", () => {
  afterEach(() => {
    cleanup();
    sidebarNodeData = null;
    lobby.enabledOnNetwork = true;
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
          <ProfileProvider initialProfile="admin">
            <Sidebar />
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Minimizar menu" }));

    expect(screen.getByRole("complementary").className).toContain("w-[68px]");
    // The menu scrolls internally even when collapsed; the submenus are `fixed`, so they are not clipped.
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
          <ProfileProvider initialProfile="admin">
            <Sidebar />
          </ProfileProvider>
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

  it("offers no destination before the node says which profile this is", () => {
    // No `ProfileProvider` pin: the profile is still the `admin` fallback, which
    // must not open the administrator's menu to an inference node.
    render(
      <MemoryRouter initialEntries={["/chat"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR">
          <Sidebar />
        </LocaleProvider>
      </MemoryRouter>,
    );
    expect(screen.queryByRole("link", { name: /Preferências administrativas/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Modelos" })).toBeNull();
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
          <ProfileProvider initialProfile="admin">
            <Sidebar />
          </ProfileProvider>
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
          <ProfileProvider initialProfile="admin">
            <Sidebar />
          </ProfileProvider>
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
   * The menu follows the same table that blocks the route (`lib/access.ts`). The Enterprise
   * screens are the discovery exception: the item stays visible without a
   * license and the screen explains how to activate it.
   */
  it("hides from the node profile the entries it cannot open and keeps Enterprise", () => {
    render(
      <MemoryRouter
        initialEntries={["/"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="worker">
            <Sidebar />
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.queryByText("Implantar na rede")).toBeNull();
    expect(screen.queryByText("Implantações")).toBeNull();
    expect(screen.getByText("Business Pack")).toBeTruthy();
    // What nobody restricted stays in place.
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
    expect(screen.getByText("Business Pack")).toBeTruthy();
    expect(screen.getByTestId("network-chat-unread").textContent).toBe("2");
  });

  it("shows an administrator the lobby's new messages beside the network chat's", () => {
    sidebarNodeData = fakeNode({ authority_level: "primary" });
    const view = () => render(
      <MemoryRouter initialEntries={["/"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="admin">
            <Sidebar />
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );
    view();
    const badge = screen.getByTestId("network-chat-lobby-unread");
    expect(badge.textContent).toBe("3");
    expect(badge.getAttribute("aria-label")).toBe("3 mensagens novas no lobby");
    expect(screen.getByTestId("network-chat-unread").textContent).toBe("2");
    cleanup();

    // No admin grant, or the lobby off: nothing to answer, no badge.
    sidebarNodeData = fakeNode({ authority_level: null });
    view();
    expect(screen.queryByTestId("network-chat-lobby-unread")).toBeNull();
    cleanup();
    sidebarNodeData = fakeNode({ authority_level: "primary" });
    lobby.enabledOnNetwork = false;
    view();
    expect(screen.queryByTestId("network-chat-lobby-unread")).toBeNull();
  });

  it("hides the TLS indicator from the flags", () => {
    sidebarNodeData = fakeNode();
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

    // The language selector is a flag with a submenu; the TLS one stays in the same group.
    const controls = screen.getByRole("button", { name: /^(Idioma|Language):/ }).parentElement as HTMLElement;
    expect(within(controls).queryByTestId("tls-indicator")).toBeNull();
  });

  it("turns the infer menu into an actionable history and leaves docs/config in the footer", () => {
    window.localStorage.setItem("pom.chat.sessions", JSON.stringify([
      {
        id: "active",
        title: "Conversa ativa",
        model: "qwen",
        messages: SPOKEN,
        updatedAt: "2026-09-07T10:00:00.000Z",
      },
      {
        id: "older",
        title: "Conversa antiga",
        model: "qwen",
        messages: SPOKEN,
        updatedAt: "2026-09-06T10:00:00.000Z",
      },
    ]));

    render(
      <MemoryRouter
        initialEntries={["/chat"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="visitor">
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
   * The history is the `infer` main menu and is seen from every route. Choosing
   * a conversation from settings or documentation changed the active conversation
   * and left the screen where it was - the menu seemed to have stopped responding.
   */
  it("goes to the chat when a conversation is chosen from another route", () => {
    window.localStorage.setItem("pom.chat.sessions", JSON.stringify([
      {
        id: "older",
        title: "Conversa antiga",
        model: "qwen",
        messages: SPOKEN,
        updatedAt: "2026-09-06T10:00:00.000Z",
      },
    ]));

    render(
      <MemoryRouter
        initialEntries={["/config/network"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="visitor">
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

  it("leaves the infer menu without history or New chat while there is no conversation", () => {
    render(
      <MemoryRouter initialEntries={["/chat"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="visitor">
            <ChatSessionProvider>
              <Sidebar />
            </ChatSessionProvider>
          </ProfileProvider>
        </LocaleProvider>
      </MemoryRouter>,
    );
    expect(screen.queryByTestId("chat-session-list")).toBeNull();
    expect(screen.queryByRole("button", { name: "Conversa nova" })).toBeNull();
    // The rest of the menu stays.
    expect(screen.getByText("Rede")).toBeTruthy();
  });

  it("keeps New chat in the infer menu outside the chat when there is no conversation", () => {
    render(
      <MemoryRouter initialEntries={["/documentation"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="visitor">
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

  it("goes to the chat when a new conversation is started from the documentation", () => {
    window.localStorage.setItem("pom.chat.sessions", JSON.stringify([
      { id: "older", title: "Conversa antiga", model: "qwen", messages: SPOKEN, updatedAt: "2026-09-06T10:00:00.000Z" },
    ]));
    render(
      <MemoryRouter
        initialEntries={["/documentation"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <ProfileProvider initialProfile="visitor">
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
