// @vitest-environment jsdom
/**
 * The defect this test guards: `clsx` concatenates, it does not resolve Tailwind
 * conflicts. While the size came from the caller's `className`, `h-6 px-1.5
 * text-[0.62rem]` and the default `h-9 px-3 text-sm` went together into the markup, and
 * whoever decided was the stylesheet order - the default. The inspector's fields
 * had asked for compact for months and rendered at full size, with no error at all
 * to give it away.
 *
 * That is why the test is about the *absence* of the default size utilities: it is
 * the stacking that caused the defect, and it would come back silent.
 */
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { Input, Select } from "./ui";

describe("compact variant", () => {
  afterEach(cleanup);

  it("swaps the Input size utilities instead of stacking them", () => {
    render(<Input compact aria-label="campo" className="text-right font-mono" />);
    const cls = screen.getByLabelText("campo").className;

    expect(cls).toContain("h-6");
    expect(cls).toContain("px-1.5");
    expect(cls).toContain("text-[0.62rem]");
    // The default size must not travel along: it was the one that won.
    expect(cls).not.toContain("h-9");
    expect(cls).not.toContain("px-3");
    expect(cls).not.toContain("text-sm");
    // What the caller sends that is not size keeps arriving.
    expect(cls).toContain("text-right");
  });

  it("keeps the default Input size without the variant", () => {
    render(<Input aria-label="campo" />);
    const cls = screen.getByLabelText("campo").className;
    expect(cls).toContain("h-9");
    expect(cls).not.toContain("h-6");
  });

  it("swaps the Select size utilities instead of stacking them", () => {
    render(<Select compact aria-label="escolha" />);
    const cls = screen.getByLabelText("escolha").className;
    expect(cls).toContain("h-6");
    expect(cls).not.toContain("h-9");
    expect(cls).not.toContain("text-sm");
  });
});
