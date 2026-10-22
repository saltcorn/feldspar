// Row history as the admin screens show it (design §14.12, TODO-history H2.3):
// the words for an entry's operation and source, who made a change, a value in
// a cell, and the changeset browser's filters as a query.
//
// The screens are `RowHistory`, `DeletedRows` and `Changesets`; what is worth
// testing about them is here.

import type { GetRowHistoryResponse, ListChangesetsQuery } from "./client";

/** One entry of a row's history, as `getRowHistory` answers it. */
export type HistoryEntry = GetRowHistoryResponse[number];
/** The changeset an entry belongs to: who, when, from where. */
export type ChangesetInfo = HistoryEntry["changeset"];

/** What an entry did to its row, in a word. */
export function opLabel(op: string): string {
  switch (op) {
    case "i":
      return "Created";
    case "u":
      return "Changed";
    case "d":
      return "Deleted";
    default:
      return op;
  }
}

/** The badge colour for an operation. */
export function opVariant(op: string): "success" | "primary" | "danger" | "secondary" {
  switch (op) {
    case "i":
      return "success";
    case "u":
      return "primary";
    case "d":
      return "danger";
    default:
      return "secondary";
  }
}

/** Where a changeset came from, in words. */
export const SOURCE_LABELS: Record<string, string> = {
  request: "Request",
  import: "CSV import",
  workflow: "Workflow",
  undo: "Undo",
  restore: "Restore",
  sync: "Offline device",
  mcp: "MCP agent",
  sql: "SQL",
};

export function sourceLabel(source: string): string {
  return SOURCE_LABELS[source] ?? source;
}

/** Who made a change: their email when the server said it, else their id. A
 * change with no user was made outside Saltcorn (`psql`) or by the system. */
export function whoLabel(changeset: ChangesetInfo): string {
  if (changeset.user_email) return changeset.user_email;
  if (changeset.user_id) return changeset.user_id;
  return changeset.source === "sql" ? "Outside Saltcorn" : "No user";
}

/** A logged value as text in a cell: nothing for null, a string as itself,
 * anything else as JSON. */
export function historyValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}

/** A time the server sent, in the admin's locale. Left as sent if it does not
 * parse. */
export function historyTime(at: string): string {
  const time = new Date(at);
  return Number.isNaN(time.getTime()) ? at : time.toLocaleString();
}

/** The changeset browser's filter boxes. */
export type ChangesetFilterForm = {
  user: string;
  table: string;
  source: string;
  since: string;
};

export const EMPTY_CHANGESET_FILTER: ChangesetFilterForm = {
  user: "",
  table: "",
  source: "",
  since: "",
};

/** The filters as the query `listChangesets` takes: blank boxes are left out,
 * and a date from the date picker is sent as the start of that day, local time. */
export function changesetQuery(
  form: ChangesetFilterForm,
  page: { limit: number; offset: number },
): ListChangesetsQuery {
  const query: ListChangesetsQuery = { limit: page.limit, offset: page.offset };
  const user = form.user.trim();
  const table = form.table.trim();
  if (user) query.user = user;
  if (table) query.table = table;
  if (form.source) query.source = form.source;
  if (form.since) {
    const day = new Date(`${form.since}T00:00:00`);
    query.since = Number.isNaN(day.getTime()) ? form.since : day.toISOString();
  }
  return query;
}

/** A row's values as `field: value` pairs for a one-line summary, the key
 * first. */
export function rowSummary(row: unknown, key?: string): string {
  if (row === null || typeof row !== "object") return "";
  const entries = Object.entries(row as Record<string, unknown>).filter(
    ([field, value]) => field !== key && value !== null && value !== undefined,
  );
  return entries.map(([field, value]) => `${field}: ${historyValue(value)}`).join(", ");
}
