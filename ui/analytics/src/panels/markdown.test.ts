import { describe, expect, it } from "vitest";

import { parseInline, parseMarkdown, plainText, safeHref } from "./markdown";

describe("Markdown for text blocks (A4.4)", () => {
  it("reads headings, paragraphs, lists, quotes, code and rules", () => {
    const doc = parseMarkdown(
      [
        "# Prices",
        "Prices rose",
        "across the city.",
        "",
        "- north",
        "- south",
        "  and east",
        "",
        "3. third",
        "4. fourth",
        "> a quote",
        "```",
        "a *b*",
        "```",
        "---",
      ].join("\n"),
    );
    expect(doc.map((b) => b.kind)).toEqual(["heading", "paragraph", "list", "list", "quote", "code", "rule"]);
    expect(doc[0]).toMatchObject({ kind: "heading", level: 1, children: [{ kind: "text", text: "Prices" }] });
    expect(doc[1]).toMatchObject({ children: [{ kind: "text", text: "Prices rose across the city." }] });
    expect(doc[2]).toMatchObject({ ordered: false, items: [[{ text: "north" }], [{ text: "south and east" }]] });
    expect(doc[3]).toMatchObject({ ordered: true, start: 3 });
    expect(doc[5]).toEqual({ kind: "code", text: "a *b*" });
  });

  it("reads strong, emphasis, code and links within a line", () => {
    expect(parseInline("a **b** *c* `d*e` [f](https://x.org)")).toEqual([
      { kind: "text", text: "a " },
      { kind: "strong", children: [{ kind: "text", text: "b" }] },
      { kind: "text", text: " " },
      { kind: "em", children: [{ kind: "text", text: "c" }] },
      { kind: "text", text: " " },
      { kind: "code", text: "d*e" },
      { kind: "text", text: " " },
      { kind: "link", href: "https://x.org", children: [{ kind: "text", text: "f" }] },
    ]);
    expect(parseInline("**bold *and em* here**")[0]).toMatchObject({
      kind: "strong",
      children: [{ text: "bold " }, { kind: "em", children: [{ text: "and em" }] }, { text: " here" }],
    });
  });

  it("leaves a column name's underscores and a lone star as written", () => {
    expect(plainText(parseInline("mean of price_per_m2 by area_m2"))).toBe("mean of price_per_m2 by area_m2");
    expect(parseInline("mean of price_per_m2")).toEqual([{ kind: "text", text: "mean of price_per_m2" }]);
    expect(parseInline("_whole_ word")[0]).toMatchObject({ kind: "em" });
    expect(parseInline("2 * 3 = 6")).toEqual([{ kind: "text", text: "2 * 3 = 6" }]);
    expect(parseInline("\\*not em\\*")).toEqual([{ kind: "text", text: "*not em*" }]);
  });

  it("links only to the web, mail and this site", () => {
    expect(safeHref("https://example.org")).toBe(true);
    expect(safeHref("mailto:a@b.org")).toBe(true);
    expect(safeHref("#/datasets/1")).toBe(true);
    expect(safeHref("javascript:alert(1)")).toBe(false);
    expect(safeHref(" JavaScript:alert(1)")).toBe(false);
    expect(safeHref("//evil.example")).toBe(false);
    expect(parseInline("[click](javascript:alert(1))")).toEqual([
      { kind: "text", text: "click" },
      { kind: "text", text: ")" },
    ]);
  });
});
