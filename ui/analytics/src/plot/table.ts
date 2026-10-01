// A summary table laid out for display (analytics TODO A2.8): the server's
// parts — the body, the Total column, the Total row and the corner — put
// into one grid of row labels, column groups and cells.
//
// The server answers each part as a small table (`r0`… the row dimensions'
// values, `c0`… the column dimensions', `n`, `v0`… the cells); this file finds
// each combination of values a place, and formats it. It is pure, so a test
// can lay out a table without a DOM.

import { formatNumber, keyOf, labelOf } from "./echarts";
import type { DataTable, TableData } from "./spec";

/** One group of columns: one combination of the column dimensions' values. */
export type ColumnGroup = {
  /** Its label for each column dimension, outermost first. */
  labels: string[];
  /** Whether it is the Total column. */
  total: boolean;
};

/** One row of the grid. */
export type ModelRow = {
  /** Its label for each row dimension, outermost first. */
  labels: string[];
  /** The cells: for each column group in order, one value per cell. */
  values: string[];
  /** Whether it is the Total row. */
  total: boolean;
};

/** A summary table, ready to draw. */
export type TableModel = {
  rowNames: string[];
  columnNames: string[];
  cellNames: string[];
  groups: ColumnGroup[];
  rows: ModelRow[];
};

type Dim = { at: number; end: number };

function dims(table: DataTable, prefix: "r" | "c", count: number): Dim[] {
  return Array.from({ length: count }, (_, i) => ({
    at: table.columns.indexOf(`${prefix}${i}`),
    end: table.columns.indexOf(`${prefix}${i}_end`),
  }));
}

function values(row: unknown[], ds: Dim[]): unknown[] {
  return ds.flatMap((d) => (d.end === -1 ? [row[d.at]] : [row[d.at], row[d.end]]));
}

function labels(row: unknown[], ds: Dim[], missing: string): string[] {
  return ds.map((d) => {
    const v = row[d.at];
    if (d.end !== -1 && typeof v === "number" && typeof row[d.end] === "number") {
      return `${formatNumber(v)}–${formatNumber(row[d.end] as number)}`;
    }
    return labelOf(v, missing);
  });
}

/** Missing last, numbers by value, the rest as text. */
function compare(a: unknown[], b: unknown[]): number {
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const [x, y] = [a[i], b[i]];
    if (x === y) continue;
    if (x === null || x === undefined) return 1;
    if (y === null || y === undefined) return -1;
    if (typeof x === "number" && typeof y === "number") return x - y;
    const o = String(x).localeCompare(String(y));
    if (o !== 0) return o;
  }
  return 0;
}

/** A cell's number: two decimals (more below 1), unlike an axis tick, which
 * rounds to what a scale needs. */
export function formatCell(v: number): string {
  const abs = Math.abs(v);
  if (abs !== 0 && (abs >= 1e12 || abs < 1e-4)) return v.toExponential(3);
  return new Intl.NumberFormat("en", {
    maximumFractionDigits: abs >= 1 || abs === 0 ? 2 : 4,
    useGrouping: abs >= 10000,
  }).format(v);
}

function cell(v: unknown, missing: string): string {
  if (typeof v === "number") return formatCell(v);
  if (v === null || v === undefined) return missing;
  return String(v);
}

/** Lay out `data`. `missing` names a missing value; `totalLabel` the totals. */
export function tableModel(data: TableData, missing = "—", totalLabel = "Total"): TableModel {
  const nr = data.rows.length;
  const nc = data.columns.length;
  const k = data.cells.length;
  const body = data.body;
  const [br, bc] = [dims(body, "r", nr), dims(body, "c", nc)];
  const firstCell = body.columns.indexOf("v0");

  // The combinations, in order: rows as the server sorted them, columns
  // sorted by their values.
  const rowCombos = new Map<string, { v: unknown[]; labels: string[] }>();
  const colCombos = new Map<string, { v: unknown[]; labels: string[] }>();
  for (const r of body.rows) {
    const rv = values(r, br);
    const cv = values(r, bc);
    if (!rowCombos.has(keyOf(rv))) rowCombos.set(keyOf(rv), { v: rv, labels: labels(r, br, missing) });
    if (!colCombos.has(keyOf(cv))) colCombos.set(keyOf(cv), { v: cv, labels: labels(r, bc, missing) });
  }
  const columns = [...colCombos.entries()].sort((a, b) => compare(a[1].v, b[1].v));
  const at = new Map<string, number>(columns.map(([key], i) => [key, i]));
  const hasTotalColumn = Boolean(data.row_totals) || (nc > 0 && nr === 0 && Boolean(data.grand_total));
  const width = (columns.length + (hasTotalColumn ? 1 : 0)) * k;

  const rows: ModelRow[] = [];
  const rowIndex = new Map<string, number>();
  for (const [key, combo] of rowCombos) {
    rowIndex.set(key, rows.length);
    rows.push({ labels: combo.labels, values: Array(width).fill(""), total: false });
  }
  for (const r of body.rows) {
    const row = rows[rowIndex.get(keyOf(values(r, br))) ?? -1];
    const group = at.get(keyOf(values(r, bc)));
    if (!row || group === undefined) continue;
    for (let i = 0; i < k; i++) row.values[group * k + i] = cell(r[firstCell + i], missing);
  }
  const fill = (row: ModelRow, group: number, source: unknown[], from: number) => {
    for (let i = 0; i < k; i++) row.values[group * k + i] = cell(source[from + i], missing);
  };
  // The Total column.
  if (hasTotalColumn) {
    const totals = data.row_totals;
    if (totals) {
      const tr = dims(totals, "r", nr);
      const from = totals.columns.indexOf("v0");
      for (const r of totals.rows) {
        const row = rows[rowIndex.get(keyOf(values(r, tr))) ?? -1];
        if (row) fill(row, columns.length, r, from);
      }
    } else if (data.grand_total && rows[0]) {
      fill(rows[0], columns.length, data.grand_total.rows[0] ?? [], data.grand_total.columns.indexOf("v0"));
    }
  }
  // The Total row: one per column group, and the corner.
  if (nr > 0 && data.grand_total) {
    const total: ModelRow = {
      labels: [totalLabel, ...Array(Math.max(nr - 1, 0)).fill("")],
      values: Array(width).fill(""),
      total: true,
    };
    const totals = data.column_totals;
    if (totals) {
      const tc = dims(totals, "c", nc);
      const from = totals.columns.indexOf("v0");
      for (const r of totals.rows) {
        const group = at.get(keyOf(values(r, tc)));
        if (group !== undefined) fill(total, group, r, from);
      }
      fill(total, columns.length, data.grand_total.rows[0] ?? [], data.grand_total.columns.indexOf("v0"));
    } else {
      fill(total, 0, data.grand_total.rows[0] ?? [], data.grand_total.columns.indexOf("v0"));
    }
    rows.push(total);
  }
  const groups: ColumnGroup[] = columns.map(([, c]) => ({ labels: c.labels, total: false }));
  if (hasTotalColumn) groups.push({ labels: [totalLabel, ...Array(Math.max(nc - 1, 0)).fill("")], total: true });
  return { rowNames: data.rows, columnNames: data.columns, cellNames: data.cells, groups, rows };
}
