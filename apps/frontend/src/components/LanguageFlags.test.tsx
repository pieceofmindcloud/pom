// @vitest-environment jsdom
import React from "react";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { LanguageFlags } from "./LanguageFlags";

afterEach(() => cleanup());

describe("LanguageFlags", () => {
  it("uses the shared flag buttons instead of a language combobox", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <LanguageFlags />
      </LocaleProvider>,
    );

    const group = screen.getByRole("group", { name: "Idioma" });
    expect(group).toBeTruthy();
    expect(screen.queryByRole("combobox")).toBeNull();
    expect(screen.getByRole("button", { name: "Português (Brasil)" }).getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "English" }));
    expect(screen.getByRole("button", { name: "English" }).getAttribute("aria-pressed")).toBe("true");
  });
});
