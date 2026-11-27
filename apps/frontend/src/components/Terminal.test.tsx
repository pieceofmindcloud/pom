// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { Terminal, terminalLineText } from "./Terminal";
import { LocaleProvider } from "../i18n/provider";

describe("Terminal", () => {
  afterEach(cleanup);

  it("draws plain and structured lines, and copies them as text", () => {
    render(
      <LocaleProvider initialLocale="en">
        <Terminal lines={["loading model", { source: "machine-a", level: "ERROR", text: "out of memory" }]} />
      </LocaleProvider>,
    );
    const lines = screen.getAllByTestId("terminal-line");
    expect(lines.map((line) => line.textContent)).toEqual(["loading model", "machine-aerrorout of memory"]);
    expect(screen.getByText("error").className).toContain("text-rose-300");
    expect(terminalLineText({ source: "a", time: "t", level: "WARN", text: "x" })).toBe("a [t] warn x");
  });

  it("says what an empty log means", () => {
    render(
      <LocaleProvider initialLocale="en">
        <Terminal lines={[]} empty={{ title: "Nothing yet", hint: "Load a model" }} />
      </LocaleProvider>,
    );
    expect(screen.getByText("Nothing yet")).toBeTruthy();
    expect(screen.getByText("Load a model")).toBeTruthy();
  });
});
