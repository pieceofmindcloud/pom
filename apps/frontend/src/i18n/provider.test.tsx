// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { LanguageSelect, LocaleProvider, useI18n } from "./provider";

function Probe() {
  const { locale } = useI18n();
  return <output>{locale}</output>;
}

describe("LocaleProvider", () => {
  it("persists a selected language and updates the document language", () => {
    localStorage.clear();
    document.documentElement.lang = "en";

    render(
      <LocaleProvider initialLocale="en">
        <LanguageSelect />
        <Probe />
      </LocaleProvider>,
    );

    fireEvent.change(screen.getByLabelText("Language"), { target: { value: "pt-BR" } });

    expect(screen.getByRole("status").textContent).toBe("pt-BR");
    expect(document.documentElement.lang).toBe("pt-BR");
    expect(localStorage.getItem("pom.locale")).toBe("pt-BR");
  });
});
