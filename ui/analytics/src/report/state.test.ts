import { describe, expect, it } from "vitest";

import { PANEL_MIME, makePanel, readPanel, readPanelDrag, setPanelDrag, type Transfer } from "../panels/panel";
import {
  BLOCK_MIME,
  addPanel,
  carriesBlock,
  dropInto,
  editBlock,
  insertBlock,
  moveBlock,
  moveBy,
  newBlock,
  readBlockDrag,
  readReport,
  removeBlock,
  setBlockDrag,
  setPage,
  type Block,
  type ReportState,
} from "./state";

const text = (s: string) => makePanel({ kind: "text", content: { markdown: s } }, s);

/** A browser's `DataTransfer`, as much of it as a drag uses. */
function transfer(): Transfer {
  const store = new Map<string, string>();
  return {
    get types() {
      return [...store.keys()];
    },
    setData: (k, v) => void store.set(k, v),
    getData: (k) => store.get(k) ?? "",
  };
}

/** What each block is, in order: a panel by its title, a heading by its text. */
function shape(r: ReportState): string[] {
  return r.blocks.map((b) =>
    b.kind === "panel" ? (b.panel.title ?? "?") : b.kind === "heading" ? `# ${b.text}` : b.kind === "text" ? `¶ ${b.markdown}` : "---",
  );
}

describe("the report as a sink (A4.3)", () => {
  it("adds dropped panels at the end or before a block, and removes them", () => {
    let r = readReport({});
    expect(r.blocks).toEqual([]);
    r = addPanel(r, text("a"));
    r = addPanel(r, text("c"));
    r = addPanel(r, text("b"), r.blocks[1].id);
    expect(shape(r)).toEqual(["a", "b", "c"]);
    r = addPanel(r, text("d"), "no-such-block");
    expect(shape(r)).toEqual(["a", "b", "c", "d"]);
    r = removeBlock(r, r.blocks[0].id);
    expect(shape(r)).toEqual(["b", "c", "d"]);
    // The state the server reads for the usage index.
    expect(r.blocks[0]).toMatchObject({ kind: "panel", panel: { kind: "text" } });
  });

  it("takes a copy when a panel is dragged from one report into another", () => {
    const dt = transfer();
    const first = addPanel(readReport({}), text("shared"));
    setPanelDrag(dt, (first.blocks[0] as Extract<Block, { kind: "panel" }>).panel);
    const copy = readPanelDrag(dt);
    if (!copy) throw new Error("a panel");
    const second = addPanel(readReport({}), copy);
    expect(second.blocks[0].id).not.toBe(first.blocks[0].id);
    expect(second.blocks[0]).toMatchObject({ panel: { content: { markdown: "shared" } } });
    expect(first.blocks).toHaveLength(1);
  });
});

describe("the report as a document (A4.4)", () => {
  it("reads every kind of block and the page, leniently", () => {
    const good = [
      { id: "h", kind: "heading", text: "Prices", level: 1 },
      { id: "t", kind: "text", markdown: "Prices *rose*." },
      { id: "p", kind: "panel", panel: text("ok") },
      { id: "b", kind: "page_break" },
    ];
    const r = readReport({
      blocks: [...good, { id: "x", kind: "panel", panel: { kind: "pie" } }, { id: "y", kind: "chart" }, { kind: "text" }, 7, null],
      page: { size: "Letter", orientation: "landscape" },
    });
    expect(r.blocks).toEqual(good);
    expect(r.page).toEqual({ size: "Letter", orientation: "landscape" });
    // A4 portrait when nothing (or nothing sensible) is stored.
    expect(readReport({}).page).toEqual({ size: "A4", orientation: "portrait" });
    expect(readReport({ page: { size: "B5", orientation: "up" } }).page).toEqual({ size: "A4", orientation: "portrait" });
    // A heading's level is 1, 2 or 3; missing text is empty.
    expect(readReport({ blocks: [{ id: "h", kind: "heading", level: 9 }] }).blocks).toEqual([
      { id: "h", kind: "heading", text: "", level: 2 },
    ]);
  });

  it("adds headings, text and page breaks, edits them, and changes the page", () => {
    let r = addPanel(readReport({}), text("plot"));
    const heading = newBlock("heading");
    const words = newBlock("text");
    r = insertBlock(r, heading, r.blocks[0].id);
    r = insertBlock(r, words, r.blocks[1].id);
    r = insertBlock(r, newBlock("page_break"));
    r = editBlock(r, heading.id, "heading", { text: "House prices", level: 1 });
    r = editBlock(r, words.id, "text", { markdown: "By area." });
    // An edit for another kind of block changes nothing.
    r = editBlock(r, words.id, "heading", { text: "not a heading" });
    expect(shape(r)).toEqual(["# House prices", "¶ By area.", "plot", "---"]);
    expect(r.blocks[0]).toMatchObject({ level: 1 });
    expect(new Set(r.blocks.map((b) => b.id)).size).toBe(4);

    r = setPage(r, { orientation: "landscape" });
    expect(r.page).toEqual({ size: "A4", orientation: "landscape" });
    // What is saved reads back as it was.
    expect(readReport(JSON.parse(JSON.stringify(r)) as Record<string, unknown>)).toEqual(r);
  });

  it("reorders blocks by dragging and by moving up and down", () => {
    let r = readReport({});
    for (const s of ["a", "b", "c", "d"]) r = addPanel(r, text(s));
    const id = (s: string) => r.blocks.find((b) => b.kind === "panel" && b.panel.title === s)!.id;
    r = moveBlock(r, id("d"), id("a"));
    expect(shape(r)).toEqual(["d", "a", "b", "c"]);
    r = moveBlock(r, id("d"));
    expect(shape(r)).toEqual(["a", "b", "c", "d"]);
    // Onto itself, or something that is not there: nothing moves.
    expect(moveBlock(r, id("b"), id("b"))).toBe(r);
    expect(moveBlock(r, "nothing", id("a"))).toBe(r);
    r = moveBy(r, id("a"), 1);
    expect(shape(r)).toEqual(["b", "a", "c", "d"]);
    r = moveBy(r, id("b"), -1);
    expect(shape(r)).toEqual(["b", "a", "c", "d"]);
    r = moveBy(r, id("d"), 1);
    expect(shape(r)).toEqual(["b", "a", "c", "d"]);
  });

  it("drags a block: moved within its report, copied into another", () => {
    let mine = readReport({});
    for (const s of ["a", "b", "c"]) mine = addPanel(mine, text(s));
    mine = insertBlock(mine, { id: "h", kind: "heading", text: "Notes", level: 2 });
    const c = mine.blocks[2];

    const dt = transfer();
    setBlockDrag(dt, "this-report", c);
    expect(carriesBlock(dt)).toBe(true);
    expect(dt.effectAllowed).toBe("copyMove");
    // A panel block goes as a panel too, for whatever else takes panels.
    expect(dt.types).toContain(PANEL_MIME);
    const dragged = readBlockDrag(dt);
    expect(dragged).toEqual({ source: "this-report", block: c });

    // Back into its own report: moved, the same block.
    const moved = dropInto(mine, "this-report", { block: dragged, panel: null }, mine.blocks[0].id);
    expect(moved && shape(moved)).toEqual(["c", "a", "b", "# Notes"]);
    expect(moved?.blocks[0].id).toBe(c.id);

    // Into another report: a copy, with a new block and panel id.
    const other = dropInto(readReport({}), "other-report", { block: dragged, panel: null });
    expect(other && shape(other)).toEqual(["c"]);
    const copied = other!.blocks[0] as Extract<Block, { kind: "panel" }>;
    expect(copied.id).not.toBe(c.id);
    expect(copied.panel.id).toBe(copied.id);
    expect(mine.blocks).toHaveLength(4);

    // A heading copied; a panel from the explorer added; anything else refused.
    const hd = transfer();
    setBlockDrag(hd, "this-report", mine.blocks[3]);
    expect(hd.types).not.toContain(PANEL_MIME);
    expect(hd.getData("text/plain")).toBe("Notes");
    const withHeading = dropInto(readReport({}), "other-report", { block: readBlockDrag(hd), panel: null });
    expect(withHeading && shape(withHeading)).toEqual(["# Notes"]);
    const withPanel = dropInto(readReport({}), "x", { block: null, panel: text("from the explorer") });
    expect(withPanel && shape(withPanel)).toEqual(["from the explorer"]);
    expect(dropInto(readReport({}), "x", { block: null, panel: null })).toBeNull();
  });

  it("drags a text block as a text panel too", () => {
    const dt = transfer();
    setBlockDrag(dt, "r", { id: "t", kind: "text", markdown: "Some *words*" });
    expect(readPanel(JSON.parse(dt.getData(PANEL_MIME)))).toMatchObject({ kind: "text", content: { markdown: "Some *words*" } });
    expect(JSON.parse(dt.getData(BLOCK_MIME))).toMatchObject({ source: "r", block: { kind: "text" } });
    dt.setData(BLOCK_MIME, "{not json");
    expect(readBlockDrag(dt)).toBeNull();
  });
});
