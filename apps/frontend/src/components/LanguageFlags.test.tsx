// @vitest-environment jsdom
import React from "react";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { LanguageFlags } from "./LanguageFlags";

afterEach(() => cleanup());

describe("LanguageFlags", () => {
  it("shows only the current language flag and opens the options on click", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <LanguageFlags />
      </LocaleProvider>,
    );

    const trigger = screen.getByRole("button", { name: "Idioma: Português (Brasil)" });
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(screen.getAllByRole("button")).toHaveLength(1);

    fireEvent.click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByRole("option", { name: "Português (Brasil)" }).getAttribute("aria-selected")).toBe("true");

    fireEvent.click(screen.getByRole("option", { name: "English" }));
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(screen.getByRole("button", { name: "Language: English" })).toBeTruthy();
  });
});
