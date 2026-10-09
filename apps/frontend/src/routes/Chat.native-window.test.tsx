// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { OpenNativeChatWindowButton } from "./Chat";
import { QuitNativeAppButton } from "../components/OpenNativeChatWindowButton";

describe("larger window control of the native chat", () => {
  it("does not appear in a regular browser without the WebKit bridge", () => {
    Object.defineProperty(window, "webkit", {
      configurable: true,
      value: undefined,
    });

    render(
      <LocaleProvider initialLocale="pt-BR">
        <OpenNativeChatWindowButton />
      </LocaleProvider>,
    );

    expect(screen.queryByRole("button", { name: /janela maior/i })).toBeNull();
  });

  it("sends the command to the WebKit handler when the app offers the bridge", () => {
    const postMessage = vi.fn();
    Object.defineProperty(window, "webkit", {
      configurable: true,
      value: { messageHandlers: { pomOpenWindow: { postMessage } } },
    });

    render(
      <LocaleProvider initialLocale="pt-BR">
        <OpenNativeChatWindowButton />
      </LocaleProvider>,
    );

    const button = screen.getByRole("button", { name: /janela maior/i });
    expect(button.textContent).toBe("");
    fireEvent.click(button);
    expect(postMessage).toHaveBeenCalledWith("open");
  });

  it("offers Quit POM only inside the macOS app, and sends the request to the bridge", () => {
    Object.defineProperty(window, "webkit", { configurable: true, value: undefined });
    const { unmount } = render(
      <LocaleProvider initialLocale="pt-BR">
        <QuitNativeAppButton />
      </LocaleProvider>,
    );
    expect(screen.queryByRole("button", { name: "Sair do POM" })).toBeNull();
    unmount();

    const postMessage = vi.fn();
    Object.defineProperty(window, "webkit", {
      configurable: true,
      value: { messageHandlers: { pomQuit: { postMessage } } },
    });
    render(
      <LocaleProvider initialLocale="pt-BR">
        <QuitNativeAppButton />
      </LocaleProvider>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Sair do POM" }));
    expect(postMessage).toHaveBeenCalledWith("quit");
  });
});
