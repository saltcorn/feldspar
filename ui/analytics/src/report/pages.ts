// A report's page and where its pages break (analytics TODO A4.5).
//
// The report is drawn on the screen at the paper's own width, in millimetres,
// so what is on the screen is what prints: the same line breaks, the plots at
// the same size (an SVG plot is drawn at a fixed size, and would be cut or
// stretched if the printed width were different).
//
// The browser breaks the pages when it prints, by the print stylesheet's
// rules: a block is not split unless it is taller than a page, a heading stays
// with what follows it, and a page break block starts a new page. `paginate`
// applies the same rules to the blocks' measured heights, so the report can
// show where the pages will break, and how many there will be, before it is
// printed.

/** The paper sizes a report can be printed on. */
export type PageSize = "A4" | "A3" | "Letter" | "Legal";
export type Orientation = "portrait" | "landscape";

/** A report's page. */
export type Page = { size: PageSize; orientation: Orientation };

export const PAGE_SIZES: PageSize[] = ["A4", "A3", "Letter", "Legal"];
export const DEFAULT_PAGE: Page = { size: "A4", orientation: "portrait" };

/** Each size's width and height in portrait, in millimetres. */
const PAPER_MM: Record<PageSize, [number, number]> = {
  A4: [210, 297],
  A3: [297, 420],
  Letter: [215.9, 279.4],
  Legal: [215.9, 355.6],
};

/** The margin on every side, in millimetres (the `@page` rules' too). */
export const MARGIN_MM = 15;

/** CSS pixels in a millimetre: 96 to the inch, on the screen and in print. */
export const PX_PER_MM = 96 / 25.4;

/** The space between two blocks, in CSS pixels (`.an-report-block`'s margin). */
export const BLOCK_GAP_PX = 24;

/** The paper's width and height in millimetres, turned for landscape. */
export function paper(page: Page): { width: number; height: number } {
  const [w, h] = PAPER_MM[page.size];
  return page.orientation === "landscape" ? { width: h, height: w } : { width: w, height: h };
}

/** The part of the page inside the margins, in millimetres. */
export function printable(page: Page): { width: number; height: number } {
  const p = paper(page);
  return { width: p.width - 2 * MARGIN_MM, height: p.height - 2 * MARGIN_MM };
}

/** The name of the stylesheet's `@page` rule for this page: `a4-landscape`. */
export function pageName(page: Page): string {
  return `${page.size.toLowerCase()}-${page.orientation}`;
}

/** A block as `paginate` needs it: its kind and its height on the paper, in
 * CSS pixels. */
export type Measured = { id: string; kind: string; height: number };

/** Where the pages break: each page's blocks, a block taller than a page on
 * every page it runs over. */
export type Pagination = {
  pages: string[][];
  /** The page each block starts on, from 1. */
  pageOf: Record<string, number>;
};

/**
 * Lay `blocks` out on pages `pageHeight` pixels high, as the print stylesheet
 * does:
 *
 * - a block that does not fit in what is left of a page starts the next one;
 *   one taller than a whole page cannot be kept whole by moving it, so it
 *   starts where it is and runs over as many pages as it needs;
 * - a heading whose next block moves to the next page goes with it;
 * - a page break ends its page, even an empty one; one at the very end
 *   makes no blank page.
 */
export function paginate(blocks: Measured[], pageHeight: number, gap = BLOCK_GAP_PX): Pagination {
  const pages: string[][] = [[]];
  const pageOf: Record<string, number> = {};
  // How far down the current page the last block ends; 0 at its top.
  let used = 0;
  const current = () => pages[pages.length - 1];
  const newPage = () => {
    pages.push([]);
    used = 0;
  };
  const fits = (height: number) => (used === 0 ? height : used + gap + height) <= pageHeight;

  blocks.forEach((block, i) => {
    if (block.kind === "page_break") {
      current().push(block.id);
      pageOf[block.id] = pages.length;
      newPage();
      return;
    }
    const height = Math.max(0, block.height);
    // Moving a block taller than a page would not keep it whole, so it stays.
    if (used > 0 && height <= pageHeight && !fits(height)) newPage();
    if (block.kind === "heading" && used > 0) {
      const next = blocks[i + 1];
      // What follows moves to the next page if it does not fit: the heading
      // goes with it. (A block taller than a page does not move.)
      if (next && next.kind !== "page_break" && next.height <= pageHeight && !fits(height + gap + next.height)) {
        newPage();
      }
    }
    current().push(block.id);
    pageOf[block.id] = pages.length;
    let end = (used === 0 ? 0 : used + gap) + height;
    // Taller than what is left of the page: on to the pages after.
    while (end > pageHeight) {
      newPage();
      current().push(block.id);
      end -= pageHeight;
    }
    used = end;
  });

  if (pages.length > 1 && current().length === 0) pages.pop();
  return { pages, pageOf };
}
