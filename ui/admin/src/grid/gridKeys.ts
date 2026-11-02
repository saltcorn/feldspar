// What a Key column shows: the row it points at, by that row's summary field.
//
// A Key field stores the target row's key — an id, mostly — which is the one
// thing about the row an admin does not recognise. Saltcorn has always shown the
// target's summary field instead (a person's name rather than `17`), and the
// grid does the same: every page of rows that arrives is followed by one read
// per Key column of the target table, `target_field=in.(…)` over the values on
// that page, and the answers are kept per column so a value seen once is never
// asked again.
//
// The stored value stays what a cell *is*: copy puts the key on the clipboard
// and a paste writes keys, so a range copied and pasted back changes nothing —
// the round trip `gridValues` keeps. The label is only what the cell shows and
// what the dropdown that edits it offers.

import { display, type RowRecord } from "./gridValues";

/** A Key column's target, as its `kind` says. `summary` is `null` when the key
 * has no summary field, and then the cell shows the key itself. */
export type KeyTarget = { table: string; field: string; summary: string | null };

/** The target of a Key field, or `null` for any other kind. */
export function keyTargetOf(kind: unknown): KeyTarget | null {
  const k = kind as {
    type?: string;
    target_table?: string;
    target_field?: string;
    summary_field?: string | null;
  } | null;
  if (k === null || typeof k !== "object" || k.type !== "key") return null;
  if (!k.target_table || !k.target_field) return null;
  return {
    table: k.target_table,
    field: k.target_field,
    summary: k.summary_field ? k.summary_field : null,
  };
}

/**
 * An `in.(…)` filter over these values, in the list syntax the server's query
 * string takes: comma-separated, with an element that has a comma, a quote or a
 * parenthesis of its own double-quoted.
 */
export function inFilter(values: string[]): string {
  const items = values.map((v) => (/[,"()]/.test(v) ? `"${v.replace(/"/g, "")}"` : v));
  return `in.(${items.join(",")})`;
}

/** The distinct non-empty values of `column` in `rows` that `known` has no
 * answer for yet — what the next label read has to ask about. */
export function unresolvedKeys(
  rows: RowRecord[],
  column: string,
  known: ReadonlyMap<string, string | null>,
): string[] {
  const out = new Set<string>();
  for (const row of rows) {
    const text = display(row[column]);
    if (text !== "" && !known.has(text)) out.add(text);
  }
  return [...out];
}

/**
 * The labels a read of the target table answers with: each target row's key, as
 * text, to its summary field's text.
 *
 * Every value that was asked about gets an entry, the ones the read did not
 * find included — as `null`, which means "asked, no such row" and stops the
 * grid asking again on every page it is on.
 */
export function labelsFrom(
  target: KeyTarget,
  asked: string[],
  rows: RowRecord[],
): Map<string, string | null> {
  const out = new Map<string, string | null>(asked.map((v) => [v, null]));
  for (const row of rows) {
    const key = display(row[target.field]);
    if (key === "") continue;
    const label = target.summary === null ? key : display(row[target.summary]);
    out.set(key, label === "" ? null : label);
  }
  return out;
}

/** What a Key cell shows: its target's label when there is one, the key itself
 * when there is not (no summary field, a dangling key, or not yet read). */
export function keyLabel(
  value: unknown,
  labels: ReadonlyMap<string, string | null> | undefined,
): string {
  const text = display(value);
  if (text === "") return "";
  return labels?.get(text) ?? text;
}
