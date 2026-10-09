import { describe, expect, it } from "vitest";

import { PANEL_MIME, makePanel, readPanelDrag, type Panel, type Transfer } from "../panels/panel";
import {
  COLUMNS,
  GAP_PX,
  ROW_PX,
  TILE_MIME,
  addTile,
  carriesTile,
  cellAt,
  collides,
  compact,
  dropInto,
  editTile,
  firstFit,
  moveTile,
  readDashboard,
  readTileDrag,
  removeTile,
  resizeTile,
  setTileDrag,
  spanOf,
  stacked,
  type DashboardState,
  type Tile,
} from "./layout";

const text = (s: string) => makePanel({ kind: "text", content: { markdown: s } }, s);
const card = (title: string): Panel =>
  makePanel({ kind: "stat_card", content: { dataset: "d1", value: { function: "count" } } }, title);

function tile(name: string, x: number, y: number, w: number, h: number): Tile {
  const panel = text(name);
  return { id: name, panel: { ...panel, id: name }, x, y, w, h };
}

/** Each tile as `name@x,y wxh`, in the order they are listed. */
function where(d: DashboardState): string[] {
  return d.tiles.map((t) => `${t.panel.title}@${t.x},${t.y} ${t.w}x${t.h}`);
}

function overlapping(d: DashboardState): boolean {
  return d.tiles.some((a, i) => d.tiles.some((b, j) => i < j && collides(a, b)));
}

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

// a a a b
// a a a b
// c c c c
const board = (): DashboardState => ({
  tiles: [tile("a", 0, 0, 9, 2), tile("b", 9, 0, 3, 2), tile("c", 0, 2, 12, 3)],
});

describe("the dashboard's grid (A6.1)", () => {
  it("reads a stored state leniently, settling tiles that overlap or float", () => {
    const d = readDashboard({
      tiles: [
        { id: "a", panel: text("a"), x: 0, y: 3, w: 6, h: 2 },
        { id: "b", panel: text("b"), x: 4, y: 3, w: 6, h: 2 },
        { id: "c", panel: text("c"), x: 20, y: 0, w: 50, h: 2 },
        { id: "x", panel: { kind: "plot" }, x: 0, y: 0, w: 1, h: 1 },
        "not a tile",
      ],
    });
    expect(where(d)).toEqual(["a@0,2 6x2", "b@4,4 6x2", "c@0,0 12x2"]);
    expect(overlapping(d)).toBe(false);
    expect(readDashboard({}).tiles).toEqual([]);
  });

  it("adds a tile in the first place it fits, at its kind's size", () => {
    let d: DashboardState = { tiles: [] };
    for (const name of ["one", "two", "three", "four", "five"]) d = addTile(d, card(name));
    // Four stat cards fill a row of twelve columns; the fifth starts the next.
    expect(where(d)).toEqual(["one@0,0 3x2", "two@3,0 3x2", "three@6,0 3x2", "four@9,0 3x2", "five@0,2 3x2"]);
    const plot = makePanel({ kind: "text", content: { markdown: "" } }, "plot");
    expect(firstFit(d.tiles, 6, 5)).toEqual({ x: 3, y: 2 });
    d = addTile(d, plot, { x: 0, y: 0 }, { w: 6, h: 5 });
    // Put at the top: what it lands on is pushed down, then everything rises.
    expect(overlapping(d)).toBe(false);
    expect(d.tiles.find((t) => t.panel.title === "plot")).toMatchObject({ x: 0, y: 0, w: 6, h: 5 });
    expect(d.tiles.find((t) => t.panel.title === "three")).toMatchObject({ x: 6, y: 0 });
    expect(d.tiles.find((t) => t.panel.title === "one")).toMatchObject({ x: 0, y: 5 });
  });

  it("moves a tile where it is put and pushes the ones in the way down", () => {
    // c to the top: a and b go below it.
    const d = moveTile(board(), "c", { x: 0, y: 0 });
    expect(where(d)).toEqual(["a@0,3 9x2", "b@9,3 3x2", "c@0,0 12x3"]);
    // b dragged to the left edge of a's row lands on a, which is pushed down.
    const e = moveTile(board(), "b", { x: 0, y: 0 });
    expect(where(e)).toEqual(["a@0,2 9x2", "b@0,0 3x2", "c@0,4 12x3"]);
    expect(overlapping(e)).toBe(false);
    // Off the grid is clamped onto it.
    expect(where(moveTile(board(), "b", { x: 11, y: 0 }))[1]).toBe("b@9,0 3x2");
  });

  it("resizes a tile within the grid and no smaller than two by two", () => {
    const d = resizeTile(board(), "a", { w: 9, h: 4 });
    expect(where(d)).toEqual(["a@0,0 9x4", "b@9,0 3x2", "c@0,4 12x3"]);
    // Wider than the columns left: as wide as fits, pushing b down.
    const e = resizeTile(board(), "a", { w: 20, h: 2 });
    expect(where(e)).toEqual(["a@0,0 12x2", "b@9,2 3x2", "c@0,4 12x3"]);
    expect(where(resizeTile(board(), "b", { w: 0, h: 1 }))[1]).toBe("b@9,0 2x2");
  });

  it("removes a tile and lets the ones below it rise; editing keeps the place", () => {
    const d = removeTile(board(), "a");
    expect(where(d)).toEqual(["b@9,0 3x2", "c@0,2 12x3"]);
    const c = compact([tile("x", 0, 5, 4, 2), tile("y", 4, 9, 4, 2)]);
    expect(c.map((t) => t.y)).toEqual([0, 0]);
    const edited = editTile(board(), "b", text("B!"));
    expect(edited.tiles[1]).toMatchObject({ id: "b", x: 9, y: 0, panel: { id: "b", title: "B!" } });
  });

  it("stacks the tiles in reading order on a narrow screen", () => {
    expect(stacked(board().tiles).map((t) => `${t.id}@${t.x},${t.y} ${t.w}x${t.h}`)).toEqual([
      "a@0,0 12x2",
      "b@0,2 12x2",
      "c@0,4 12x3",
    ]);
  });

  it("finds the cell under the pointer and the span of a size in pixels", () => {
    const grid = { width: 12 * 50 + 11 * GAP_PX, columns: COLUMNS };
    expect(cellAt(grid, 0, 0)).toEqual({ x: 0, y: 0 });
    expect(cellAt(grid, 50 + GAP_PX + 1, ROW_PX + GAP_PX + 1)).toEqual({ x: 1, y: 1 });
    expect(cellAt(grid, 10_000, -5)).toEqual({ x: 11, y: 0 });
    expect(spanOf(grid, 3 * 50 + 2 * GAP_PX, 2 * ROW_PX + GAP_PX)).toEqual({ w: 3, h: 2 });
    expect(spanOf(grid, 1, 1)).toEqual({ w: 1, h: 1 });
  });
});

describe("dragging tiles (A6.1)", () => {
  it("carries the tile and its panel, so a report takes it too", () => {
    const tr = transfer();
    const b = board().tiles[1];
    setTileDrag(tr, "me", b);
    expect(carriesTile(tr)).toBe(true);
    expect(tr.types).toContain(PANEL_MIME);
    expect(readTileDrag(tr)).toEqual({ source: "me", tile: b });
    const copied = readPanelDrag(tr);
    expect(copied?.title).toBe("b");
    expect(copied?.id).not.toBe(b.panel.id);
    expect(readTileDrag({ getData: () => "{" })).toBeNull();
    expect(tr.types).toContain(TILE_MIME);
  });

  it("moves its own tile, copies another dashboard's and adds a panel", () => {
    const tr = transfer();
    const b = board().tiles[1];
    setTileDrag(tr, "me", b);
    const dropped = { tile: readTileDrag(tr), panel: null };

    // Into the same dashboard: moved.
    const moved = dropInto(board(), "me", dropped, { x: 0, y: 5 }) as DashboardState;
    expect(moved.tiles).toHaveLength(3);
    expect(moved.tiles.find((t) => t.id === "b")).toMatchObject({ x: 0, y: 5 });
    // A narrow screen has no cells: its own tile stays.
    expect(dropInto(board(), "me", dropped)).toEqual(board());

    // Into another: copied at its size, with ids of its own.
    const copied = dropInto(board(), "other", dropped, { x: 0, y: 0 }) as DashboardState;
    expect(copied.tiles).toHaveLength(4);
    const copy = copied.tiles[3];
    expect(copy).toMatchObject({ x: 0, y: 0, w: 3, h: 2, panel: { title: "b" } });
    expect(copy.id).not.toBe("b");
    expect(copy.panel.id).not.toBe("b");
    expect(overlapping(copied)).toBe(false);

    // A panel from anywhere: added where it is dropped, at its kind's size.
    const added = dropInto(board(), "me", { tile: null, panel: card("n") }, { x: 3, y: 1 }) as DashboardState;
    // It lands on a, which is pushed below it; with nothing above, it rises to the top.
    expect(added.tiles[3]).toMatchObject({ x: 3, y: 0, w: 3, h: 2 });
    expect(added.tiles[0]).toMatchObject({ id: "a", y: 2 });
    expect(overlapping(added)).toBe(false);
    expect(dropInto(board(), "me", { tile: null, panel: null })).toBeNull();
  });
});
