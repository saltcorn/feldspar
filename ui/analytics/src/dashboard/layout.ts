// The Dashboard workspace's state and its grid (analytics TODO A6.1).
//
// A dashboard is tiles of panels on a grid `COLUMNS` wide: each tile is
// `{ id, panel, x, y, w, h }`, `x` and `w` in columns and `y` and `h` in rows
// of `ROW_PX`. The server reads the same shape
// (`sc_analytics::panel::check_state`): the tiles' panels are what the usage
// index finds, and tiles off the grid or overlapping are refused.
//
// **The layout settles.** A tile moved or resized keeps where it was put, and
// the tiles it lands on are pushed down out of its way; then every tile rises
// as far as it can, so there are no holes. A narrow screen shows the same
// tiles one above the other, in reading order (`stacked`); what is stored is
// always the wide layout.
//
// **Tiles are dragged.** A tile dragged within its dashboard moves to the cell
// it is dropped on; dragged into another dashboard it is copied there, as a
// panel always is; its panel goes too, so a report takes it (`setTileDrag`).

import { copyPanel, newPanelId, readPanel, setPanelDrag, type Panel, type PanelKind, type Transfer } from "../panels/panel";
import { readDrill, type Drill } from "./drill";
import { readFilters, readRefresh, type Condition } from "./filters";

/** The grid's columns. */
export const COLUMNS = 12;
/** The height of one row of the grid, in pixels. */
export const ROW_PX = 64;
/** The gap between tiles, in pixels. */
export const GAP_PX = 12;
/** The most rows a tile may span (as the server says). */
export const MAX_ROWS = 40;
/** Below this width, in pixels, the tiles stack in one column. */
export const NARROW_PX = 640;

/** One tile; a plot's may have a drill path (A6.5). */
export type Tile = { id: string; panel: Panel; x: number; y: number; w: number; h: number; drill?: Drill };

/** A dashboard's state: its tiles, its own filters (A6.6) and how often it
 * refreshes, in seconds (0, or absent, for never). */
export type DashboardState = { tiles: Tile[]; filters?: Condition[]; refresh?: number };

/** Where a tile is, without its panel. */
export type Rect = { x: number; y: number; w: number; h: number };

/** The size a new tile of a kind is given, in columns and rows. */
export function defaultSize(kind: PanelKind | undefined): { w: number; h: number } {
  switch (kind) {
    case "stat_card":
      return { w: 3, h: 2 };
    case "text":
      return { w: 4, h: 3 };
    case "map":
      return { w: 6, h: 6 };
    case "fit_table":
    case "summary_table":
      return { w: 6, h: 4 };
    default:
      return { w: 6, h: 5 };
  }
}

/** The smallest a tile may be, in columns and rows. */
export const MIN_SIZE = { w: 2, h: 2 };

function isObject(v: unknown): v is Record<string, unknown> {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}

function whole(v: unknown, fallback: number): number {
  return typeof v === "number" && Number.isFinite(v) ? Math.max(0, Math.round(v)) : fallback;
}

/** A rectangle made to fit the grid. */
export function clamp(r: Rect): Rect {
  const w = Math.min(COLUMNS, Math.max(1, Math.round(r.w)));
  const h = Math.min(MAX_ROWS, Math.max(1, Math.round(r.h)));
  const x = Math.min(COLUMNS - w, Math.max(0, Math.round(r.x)));
  const y = Math.max(0, Math.round(r.y));
  return { x, y, w, h };
}

/** One tile read from anything, or `null`. */
export function readTile(raw: unknown): Tile | null {
  if (!isObject(raw) || typeof raw.id !== "string" || raw.id === "") return null;
  const panel = readPanel(raw.panel);
  if (!panel) return null;
  const size = defaultSize(panel.kind);
  const tile: Tile = { id: raw.id, panel, ...clamp({ x: whole(raw.x, 0), y: whole(raw.y, 0), w: whole(raw.w, size.w), h: whole(raw.h, size.h) }) };
  const drill = panel.kind === "plot" ? readDrill(raw.drill) : null;
  if (drill) tile.drill = drill;
  return tile;
}

/** The state a workspace stored, read leniently and settled: what is not a
 * tile is dropped, and tiles that overlap are moved apart. */
export function readDashboard(raw: Record<string, unknown>): DashboardState {
  const tiles = Array.isArray(raw.tiles) ? raw.tiles : [];
  const filters = readFilters(raw.filters);
  const refresh = readRefresh(raw.refresh);
  return {
    ...(filters.length > 0 ? { filters } : {}),
    ...(refresh > 0 ? { refresh } : {}),
    tiles: compact(
      separate(
        tiles.flatMap((t) => {
          const tile = readTile(t);
          return tile ? [tile] : [];
        }),
      ),
    ),
  };
}

/** Whether two rectangles share a cell. */
export function collides(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

/** Reading order: top to bottom, then left to right. */
function byPosition(a: Rect, b: Rect): number {
  return a.y - b.y || a.x - b.x;
}

/** The tiles with each one, in reading order, moved down below whatever it
 * overlaps that was placed before it; `first` (a tile's id) is placed before
 * all of them and stays where it is. The order of the list is kept. */
function separate(tiles: Tile[], first?: string): Tile[] {
  const fixed = tiles.find((t) => t.id === first);
  const rest = tiles.filter((t) => t !== fixed).sort(byPosition);
  const placed: Tile[] = fixed ? [fixed] : [];
  for (const tile of rest) {
    let y = tile.y;
    for (;;) {
      const hit = placed.filter((p) => collides({ ...tile, y }, p));
      if (hit.length === 0) break;
      y = Math.max(...hit.map((p) => p.y + p.h));
    }
    placed.push(y === tile.y ? tile : { ...tile, y });
  }
  return tiles.map((t) => placed.find((p) => p.id === t.id) as Tile);
}

/** The tiles, each risen as far as it can in reading order: no holes above
 * any of them. The order of the list is kept. */
export function compact(tiles: Tile[]): Tile[] {
  const risen = new Map<string, Tile>();
  const done: Tile[] = [];
  for (const tile of [...tiles].sort(byPosition)) {
    let y = tile.y;
    while (y > 0 && !done.some((d) => collides({ ...tile, y: y - 1 }, d))) y -= 1;
    const out = y === tile.y ? tile : { ...tile, y };
    done.push(out);
    risen.set(tile.id, out);
  }
  return tiles.map((t) => risen.get(t.id) ?? t);
}

/** The tiles settled around `id`: it stays where it is, those it lands on are
 * pushed down, and then everything rises. */
export function settle(tiles: Tile[], id: string): Tile[] {
  return compact(separate(tiles, id));
}

/** The dashboard with the tile `id` moved to `to` (its top-left cell). */
export function moveTile(state: DashboardState, id: string, to: { x: number; y: number }): DashboardState {
  const tiles = state.tiles.map((t) => (t.id === id ? { ...t, ...clamp({ ...t, ...to }) } : t));
  return { ...state, tiles: settle(tiles, id) };
}

/** The dashboard with the tile `id` made `size` (in columns and rows). */
export function resizeTile(state: DashboardState, id: string, size: { w: number; h: number }): DashboardState {
  const tiles = state.tiles.map((t) => {
    if (t.id !== id) return t;
    const w = Math.max(MIN_SIZE.w, Math.min(size.w, COLUMNS - t.x));
    const h = Math.max(MIN_SIZE.h, size.h);
    return { ...t, ...clamp({ x: t.x, y: t.y, w, h }) };
  });
  return { ...state, tiles: settle(tiles, id) };
}

/** The first place a `w` by `h` tile fits without moving anything: the
 * highest row, then the leftmost column. */
export function firstFit(tiles: Rect[], w: number, h: number): { x: number; y: number } {
  const bottom = tiles.reduce((m, t) => Math.max(m, t.y + t.h), 0);
  for (let y = 0; y <= bottom; y += 1) {
    for (let x = 0; x + w <= COLUMNS; x += 1) {
      if (!tiles.some((t) => collides({ x, y, w, h }, t))) return { x, y };
    }
  }
  return { x: 0, y: bottom };
}

/** The dashboard with `panel` in a new tile: at `at` (pushing what is there
 * down), or in the first place it fits. The panel is the dashboard's own copy
 * already (`readPanelDrag`). */
export function addTile(
  state: DashboardState,
  panel: Panel,
  at?: { x: number; y: number },
  size?: { w: number; h: number },
): DashboardState {
  const { w, h } = size ?? defaultSize(panel.kind);
  const where = at ?? firstFit(state.tiles, w, h);
  const tile: Tile = { id: panel.id, panel, ...clamp({ ...where, w, h }) };
  return { ...state, tiles: settle([...state.tiles, tile], tile.id) };
}

/** The dashboard with the tile `id` given a drill path, or (`null`) none. */
export function setDrill(state: DashboardState, id: string, drill: Drill | null): DashboardState {
  return {
    ...state,
    tiles: state.tiles.map((t) => {
      if (t.id !== id) return t;
      const out: Tile = { ...t };
      if (drill) out.drill = drill;
      else delete out.drill;
      return out;
    }),
  };
}

/** The dashboard with the tile `id`'s panel replaced (its place stays). */
export function editTile(state: DashboardState, id: string, panel: Panel): DashboardState {
  return { ...state, tiles: state.tiles.map((t) => (t.id === id ? { ...t, panel: { ...panel, id: t.panel.id } } : t)) };
}

/** The dashboard without the tile `id`; the tiles below it rise. */
export function removeTile(state: DashboardState, id: string): DashboardState {
  return { ...state, tiles: compact(state.tiles.filter((t) => t.id !== id)) };
}

/** The tiles one above the other, full width, in reading order: a narrow
 * screen's layout of the same dashboard. */
export function stacked(tiles: Tile[]): Tile[] {
  let y = 0;
  return [...tiles].sort(byPosition).map((t) => {
    const out = { ...t, x: 0, w: COLUMNS, y };
    y += t.h;
    return out;
  });
}

/** How many rows the tiles take. */
export function rowsOf(tiles: Rect[]): number {
  return tiles.reduce((m, t) => Math.max(m, t.y + t.h), 0);
}

/** The grid's geometry at a width, in pixels. */
export type Grid = { width: number; columns: number };

/** The width of one column at a grid width. */
export function columnWidth(grid: Grid): number {
  return (grid.width - GAP_PX * (grid.columns - 1)) / grid.columns;
}

/** The cell a point in the grid (pixels from its top-left corner) is in. */
export function cellAt(grid: Grid, px: number, py: number): { x: number; y: number } {
  const col = columnWidth(grid) + GAP_PX;
  return {
    x: Math.min(grid.columns - 1, Math.max(0, Math.floor(px / col))),
    y: Math.max(0, Math.floor(py / (ROW_PX + GAP_PX))),
  };
}

/** The size in columns and rows nearest to a size in pixels. */
export function spanOf(grid: Grid, width: number, height: number): { w: number; h: number } {
  const col = columnWidth(grid) + GAP_PX;
  return {
    w: Math.max(1, Math.round((width + GAP_PX) / col)),
    h: Math.max(1, Math.round((height + GAP_PX) / (ROW_PX + GAP_PX))),
  };
}

// --- dragging tiles ---------------------------------------------------------------

/** The drag data type a dashboard's tile travels as. */
export const TILE_MIME = "application/x-feldspar-dashboard-tile";

/** A dragged tile, and the dashboard (one open copy of it) it came from. */
export type TileDrag = { source: string; tile: Tile };

/** Start dragging `tile` out of the dashboard `source`; its panel goes too,
 * for whatever takes panels. */
export function setTileDrag(transfer: Transfer, source: string, tile: Tile): void {
  setPanelDrag(transfer, tile.panel);
  transfer.setData(TILE_MIME, JSON.stringify({ source, tile }));
  transfer.effectAllowed = "copyMove";
}

/** Whether a drag carries a dashboard's tile. */
export function carriesTile(transfer: Pick<Transfer, "types">): boolean {
  return Array.from(transfer.types).includes(TILE_MIME);
}

/** The tile a drop carries, as it was dragged — or `null`. */
export function readTileDrag(transfer: Pick<Transfer, "getData">): TileDrag | null {
  try {
    const raw: unknown = JSON.parse(transfer.getData(TILE_MIME));
    if (!isObject(raw) || typeof raw.source !== "string") return null;
    const tile = readTile(raw.tile);
    return tile ? { source: raw.source, tile } : null;
  } catch {
    return null;
  }
}

/** The dashboard after a drop on the cell `at` of the dashboard `self`: its
 * own tile moves there, another dashboard's tile is copied there at its size,
 * a panel from anywhere is added there. Without `at` (a narrow screen's
 * stacked tiles have no cells) what arrives goes in the first place it fits
 * and the dashboard's own tile stays. `null` when the drop carries nothing a
 * dashboard takes. */
export function dropInto(
  state: DashboardState,
  self: string,
  dropped: { tile: TileDrag | null; panel: Panel | null },
  at?: { x: number; y: number },
): DashboardState | null {
  if (dropped.tile) {
    const { source, tile } = dropped.tile;
    if (source === self && state.tiles.some((t) => t.id === tile.id)) return at ? moveTile(state, tile.id, at) : state;
    const copy = copyPanel(tile.panel);
    const added = addTile(state, copy, at, { w: tile.w, h: tile.h });
    return tile.drill ? setDrill(added, copy.id, tile.drill) : added;
  }
  return dropped.panel ? addTile(state, dropped.panel, at) : null;
}

/** A new tile's id, for a panel made here (a stat card, some text). */
export function newTileId(): string {
  return newPanelId();
}
