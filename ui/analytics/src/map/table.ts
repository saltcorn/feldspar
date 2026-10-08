// The Map workspace's attribute table (analytics TODO A5.10), as data: the
// rows `layerRows` answered, put in the order the table shows them.
//
// The server sorts a layer whose rows are a table's (`sorted`), since its ids
// are the rows' keys whatever the order; any other layer's ids are places in
// its dataset's order, so it is read in that order and sorted here. Either
// way the ids travel with their rows, which is what the selection follows.

import type { StageColumn } from "../datasets/ops";

/** What `layerRows` answers. */
export type LayerRows = {
  columns: StageColumn[];
  rows: unknown[][];
  ids: unknown[];
  total: number;
  keyed: boolean;
  sorted: boolean;
};

/** The table's order. */
export type TableSort = { column: string; descending: boolean };

/** Compare two cells: missing values last, numbers as numbers, the rest as
 * text in the reader's language. */
export function compareCells(a: unknown, b: unknown): number {
  const missingA = a === null || a === undefined;
  const missingB = b === null || b === undefined;
  if (missingA || missingB) return missingA === missingB ? 0 : missingA ? 1 : -1;
  if (typeof a === "number" && typeof b === "number") return a - b;
  if (typeof a === "boolean" && typeof b === "boolean") return Number(a) - Number(b);
  return String(a).localeCompare(String(b), undefined, { numeric: true });
}

/** The rows' places in the order the table shows them: as answered when the
 * server sorted them (or nothing asks for an order), else sorted here —
 * stably, so ties keep the dataset's order. */
export function tableOrder(answer: LayerRows, sort: TableSort | undefined): number[] {
  const order = answer.rows.map((_, i) => i);
  if (!sort || answer.sorted) return order;
  const at = answer.columns.findIndex((c) => c.name === sort.column);
  if (at < 0) return order;
  const sign = sort.descending ? -1 : 1;
  return order.sort((i, j) => {
    const a = answer.rows[i][at];
    const b = answer.rows[j][at];
    const missing = (a === null || a === undefined) !== (b === null || b === undefined);
    // Missing values last either way.
    const c = missing ? compareCells(a, b) : sign * compareCells(a, b);
    return c !== 0 ? c : i - j;
  });
}

/** The selected rows' places, in the table's order: the rows a "show the
 * selection only" table keeps. */
export function selectedOnly(answer: LayerRows, order: number[], selected: unknown[]): number[] {
  const set = new Set(selected);
  return order.filter((i) => set.has(answer.ids[i]));
}
