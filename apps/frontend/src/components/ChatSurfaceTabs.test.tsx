// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { LocaleProvider } from "../i18n/provider";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it } from "vitest";
import { ChatSurfaceTabs } from "./ChatSurfaceTabs";

function mount(initialPath: string) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <MemoryRouter initialEntries={[initialPath]}>
        <ChatSurfaceTabs />
        <Routes>
          <Route path="/chat" element={<p>chat de IA</p>} />
          <Route path="/network-chat" element={<p>chat da rede</p>} />
        </Routes>
      </MemoryRouter>
    </LocaleProvider>,
  );
}

describe("abas de superfície do chat", () => {
  afterEach(cleanup);

  it("volta do Chat da rede para o Chat de IA", () => {
    mount("/network-chat");

    expect(screen.getByRole("tab", { name: "Rede" }).getAttribute("aria-selected")).toBe("true");
    fireEvent.click(screen.getByRole("tab", { name: "IA" }));

    expect(screen.getByText("chat de IA")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "IA" }).getAttribute("aria-selected")).toBe("true");
  });

  it("vai do Chat de IA para o Chat da rede", () => {
    mount("/chat");

    fireEvent.click(screen.getByRole("tab", { name: "Rede" }));

    expect(screen.getByText("chat da rede")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Rede" }).getAttribute("aria-selected")).toBe("true");
  });
});
