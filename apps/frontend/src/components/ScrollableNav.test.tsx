// @vitest-environment jsdom
import React from "react";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { ScrollableNav } from "./Sidebar";

afterEach(() => cleanup());

describe("ScrollableNav", () => {
  it("shows only the arrows that point to hidden items and scrolls on click", () => {
    const { container } = render(
      <LocaleProvider initialLocale="en">
        <ScrollableNav>
          <a href="#one">One</a>
          <a href="#two">Two</a>
        </ScrollableNav>
      </LocaleProvider>,
    );
    const scroller = container.querySelector(".overflow-y-auto") as HTMLElement;
    // A 200px window in front of 500px of content, at the top.
    Object.defineProperty(scroller, "clientHeight", { configurable: true, value: 200 });
    Object.defineProperty(scroller, "scrollHeight", { configurable: true, value: 500 });
    Object.defineProperty(scroller, "scrollTop", { configurable: true, writable: true, value: 0 });
    scroller.scrollBy = vi.fn();

    fireEvent.scroll(scroller);
    expect(screen.queryByRole("button", { name: "Scroll menu up" })).toBeNull();
    const down = screen.getByRole("button", { name: "Scroll menu down" });

    fireEvent.click(down);
    expect(scroller.scrollBy).toHaveBeenCalledWith({ top: 120, behavior: "smooth" });

    // Scrolled to the end: the down arrow disappears and the up arrow appears.
    Object.defineProperty(scroller, "scrollTop", { configurable: true, writable: true, value: 300 });
    fireEvent.scroll(scroller);
    expect(screen.queryByRole("button", { name: "Scroll menu down" })).toBeNull();
    expect(screen.getByRole("button", { name: "Scroll menu up" })).toBeTruthy();
  });

  it("shows no arrows when everything fits", () => {
    const { container } = render(
      <LocaleProvider initialLocale="en">
        <ScrollableNav>
          <a href="#one">One</a>
        </ScrollableNav>
      </LocaleProvider>,
    );
    const scroller = container.querySelector(".overflow-y-auto") as HTMLElement;
    Object.defineProperty(scroller, "clientHeight", { configurable: true, value: 300 });
    Object.defineProperty(scroller, "scrollHeight", { configurable: true, value: 300 });
    fireEvent.scroll(scroller);
    expect(screen.queryByRole("button", { name: "Scroll menu up" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Scroll menu down" })).toBeNull();
  });
});
