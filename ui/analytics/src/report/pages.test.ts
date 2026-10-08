import { describe, expect, it } from "vitest";

import { BLOCK_GAP_PX, PX_PER_MM, pageName, paginate, paper, printable, type Measured } from "./pages";
import { printMarks, panelsDrawn } from "./print";

const block = (id: string, height: number, kind = "panel"): Measured => ({ id, kind, height });
const gap = BLOCK_GAP_PX;

describe("the page (A4.5)", () => {
  it("knows each paper's size, turned for landscape, and the part inside the margins", () => {
    expect(paper({ size: "A4", orientation: "portrait" })).toEqual({ width: 210, height: 297 });
    expect(paper({ size: "A4", orientation: "landscape" })).toEqual({ width: 297, height: 210 });
    expect(paper({ size: "Letter", orientation: "portrait" })).toEqual({ width: 215.9, height: 279.4 });
    expect(printable({ size: "A3", orientation: "landscape" })).toEqual({ width: 390, height: 267 });
    // An A4 page's inside is 180 by 267 mm: about 680 by 1009 CSS pixels.
    expect(Math.round(printable({ size: "A4", orientation: "portrait" }).height * PX_PER_MM)).toBe(1009);
    expect(pageName({ size: "Legal", orientation: "landscape" })).toBe("legal-landscape");
  });

  it("marks the report for printing on its own named page, titled by its name", () => {
    expect(printMarks({ size: "A4", orientation: "landscape" }, "Quarterly", "Report")).toEqual({
      root: ["an-print-root", "an-page-a4-landscape"],
      html: ["an-printing"],
      title: "Quarterly",
    });
    expect(printMarks({ size: "Letter", orientation: "portrait" }, "  ", "Report").title).toBe("Report");
  });

  it("waits for panels still loading", () => {
    const root = (loading: boolean) => ({ querySelector: (s: string) => (loading && s === ".an-panel-loading" ? {} : null) });
    expect(panelsDrawn(root(true) as unknown as ParentNode)).toBe(false);
    expect(panelsDrawn(root(false) as unknown as ParentNode)).toBe(true);
  });
});

describe("pagination (A4.5)", () => {
  it("puts blocks on a page while they fit, with the gap between them", () => {
    const p = paginate([block("a", 400), block("b", 400), block("c", 400)], 1000);
    // a and b: 400 + 24 + 400 = 824; c would end at 1248.
    expect(p.pages).toEqual([["a", "b"], ["c"]]);
    expect(p.pageOf).toEqual({ a: 1, b: 1, c: 2 });
    // Exactly full is full, not over.
    expect(paginate([block("a", 500 - gap / 2), block("b", 500 - gap / 2)], 1000).pages).toEqual([["a", "b"]]);
  });

  it("starts a new page at a page break, and makes no blank page for one at the end", () => {
    const p = paginate([block("a", 100), block("br", 30, "page_break"), block("b", 100), block("end", 30, "page_break")], 1000);
    expect(p.pages).toEqual([["a", "br"], ["b", "end"]]);
    expect(p.pageOf.b).toBe(2);
    // Two breaks in a row leave a page with nothing on it.
    const twice = paginate([block("a", 100), block("x", 0, "page_break"), block("y", 0, "page_break"), block("b", 100)], 1000);
    expect(twice.pages).toEqual([["a", "x"], ["y"], ["b"]]);
  });

  it("runs a block taller than a page over as many pages as it needs, from where it is", () => {
    const p = paginate([block("a", 100), block("tall", 2500), block("b", 100)], 1000);
    // Moving it would not keep it whole: it starts under a (at 124), ends 624
    // down the third page, and b follows it there.
    expect(p.pages).toEqual([["a", "tall"], ["tall"], ["tall", "b"]]);
    expect(p.pageOf).toEqual({ a: 1, tall: 1, b: 3 });
    expect(paginate([block("tall", 1500)], 1000).pages).toEqual([["tall"], ["tall"]]);
  });

  it("keeps a heading with what follows it", () => {
    const blocks = [block("a", 800), block("h", 40, "heading"), block("plot", 400)];
    // The heading fits under a (800 + 24 + 40 = 864) but the plot does not: both move.
    expect(paginate(blocks, 1000).pages).toEqual([["a"], ["h", "plot"]]);
    // With room for both, both stay.
    expect(paginate([block("a", 400), ...blocks.slice(1)], 1000).pages).toEqual([["a", "h", "plot"]]);
    // A heading followed by a page break, or by nothing, stays where it is.
    expect(paginate([block("a", 800), block("h", 40, "heading"), block("br", 0, "page_break")], 1000).pages).toEqual([
      ["a", "h", "br"],
    ]);
    expect(paginate([block("a", 800), block("h", 40, "heading")], 1000).pages).toEqual([["a", "h"]]);
    // Followed by a block taller than a page, which does not move, it stays.
    expect(paginate([block("a", 900), block("h", 40, "heading"), block("tall", 3000)], 1000).pages[0]).toEqual(["a", "h", "tall"]);
  });

  it("is one empty page for an empty report", () => {
    expect(paginate([], 1000)).toEqual({ pages: [[]], pageOf: {} });
  });
});
