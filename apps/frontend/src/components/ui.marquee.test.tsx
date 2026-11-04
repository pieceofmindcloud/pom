// @vitest-environment jsdom
import { render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MarqueeText } from "./ui";

/** jsdom lays nothing out: the box and the text get the widths each test names. */
function layout(box: number, text: number) {
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("pom-marquee") ? box : 0;
  });
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("pom-marquee-copy") ? text : 0;
  });
}

describe("MarqueeText", () => {
  afterEach(() => vi.restoreAllMocks());

  it("leaves a text that fits still, in one copy", () => {
    layout(200, 120);
    const { container } = render(<MarqueeText>ksia-gpus</MarqueeText>);
    expect(container.querySelector(".pom-marquee-on")).toBeNull();
    expect(container.querySelectorAll(".pom-marquee-copy")).toHaveLength(1);
  });

  it("scrolls a text that does not fit, with a hidden second copy closing the lap", () => {
    layout(120, 300);
    const name = "a-machine-name-far-longer-than-the-tab";
    const { container } = render(<MarqueeText>{name}</MarqueeText>);
    const box = container.querySelector(".pom-marquee-on") as HTMLElement;
    expect(box).not.toBeNull();
    expect(box.title).toBe(name);
    const copies = container.querySelectorAll(".pom-marquee-copy");
    expect(copies).toHaveLength(2);
    expect(copies[1].getAttribute("aria-hidden")).toBe("true");
    const track = container.querySelector(".pom-marquee-track") as HTMLElement;
    expect(track.style.getPropertyValue("--marquee-shift")).toBe("-332px"); // the text and the gap
  });
});
