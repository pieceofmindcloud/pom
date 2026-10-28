/**
 * The markdown format of the guides (`scripts/guides-md.mjs`): every block a
 * guide can hold comes out of the file as the data `Guide.tsx` renders, and a
 * malformed file stops the build instead of rendering a broken page.
 */
import { describe, expect, it } from "vitest";
// @ts-expect-error - a plain Node module, run by the build without types.
import { localeOf, parseGuide } from "../../scripts/guides-md.mjs";

const FRONT = "---\nid: g\nlabel: G\nsummary: S\ntitle: T\norder: 5\n---\n";

describe("guide markdown", () => {
  it("reads every block", () => {
    const text = `${FRONT}
:::lead
Lead
text.
:::

A paragraph with \`code\`.

## Heading

:::steps
### One
First.
### Two
Second.
:::

:::note Title
Body.
:::

| a | b |
|---|---|
| 1 | x \\| y |

:::diagram topology
Caption.
:::

:::stats
- **32** layers
:::

:::faq
### Why?
Because.
:::

![Shot](../images/en/x.webp)

:::video https://www.youtube.com/watch?v=lCYW3TJ70M8
Watch.
:::

:::deep docs/subsystems/slicing.md
Deeper
:::
`;
    const { guide, order, images } = parseGuide(text, "docs/guides/screens/g.md");
    expect(order).toBe(5);
    expect(images).toEqual(["docs/guides/images/en/x.webp"]);
    expect(guide.blocks).toEqual([
      { kind: "lead", text: "Lead text." },
      { kind: "p", text: "A paragraph with `code`." },
      { kind: "h", text: "Heading" },
      { kind: "steps", items: [{ title: "One", text: "First." }, { title: "Two", text: "Second." }] },
      { kind: "note", title: "Title", text: "Body." },
      { kind: "table", head: ["a", "b"], rows: [["1", "x | y"]] },
      { kind: "diagram", id: "topology", caption: "Caption." },
      { kind: "stats", items: [{ value: "32", label: "layers" }] },
      { kind: "faq", items: [{ q: "Why?", a: "Because." }] },
      { kind: "image", src: "docs/guides/images/en/x.webp", caption: "Shot" },
      { kind: "video", youtube: "lCYW3TJ70M8", caption: "Watch." },
      { kind: "deep", doc: "docs/subsystems/slicing.md", text: "Deeper" },
    ]);
  });

  it("refuses what it does not know", () => {
    expect(() => parseGuide(`${FRONT}:::nope\nx\n:::\n`, "f.md")).toThrow(/unknown directive/);
    expect(() => parseGuide(`${FRONT}:::note T\nnever closed\n`, "f.md")).toThrow(/not closed/);
    expect(() => parseGuide(`${FRONT}:::video https://example.com/v\nx\n:::\n`, "f.md")).toThrow(/YouTube/);
    expect(() => parseGuide("---\nid: g\n---\nbody\n", "f.md")).toThrow(/missing "label"/);
  });

  it("the language is in the file name", () => {
    expect(localeOf("docs/guides/screens/chat.md")).toBe("en");
    expect(localeOf("docs/guides/screens/chat.pt-BR.md")).toBe("pt-BR");
  });
});
