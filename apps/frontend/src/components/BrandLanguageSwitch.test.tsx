// @vitest-environment jsdom
import React from "react";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { BrandLanguageSwitch } from "./BrandLanguageSwitch";

afterEach(() => cleanup());

describe("BrandLanguageSwitch", () => {
  it("shows the current language and switches it from the submenu", () => {
    render(
      <LocaleProvider initialLocale="en">
        <BrandLanguageSwitch />
      </LocaleProvider>,
    );

    const trigger = screen.getByRole("button", { name: "Language: English" });
    expect(trigger.textContent).toContain("EN");
    expect(screen.getAllByRole("button")).toHaveLength(1);

    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole("option", { name: "Português (Brasil)" }));

    expect(screen.queryByRole("listbox")).toBeNull();
    expect(screen.getByRole("button", { name: "Idioma: Português (Brasil)" }).textContent).toContain("PT");
  });
});
