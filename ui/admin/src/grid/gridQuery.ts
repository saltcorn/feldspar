// What the data grid asks the server for: a filter, an order and a page.
//
// The grid is a spreadsheet over a table of any size, so it never holds the
// table — it holds the rows under the viewport. Everything about *which* rows
// those are is arithmetic and string-building over what the header says and what
// the filter row has been typed into, so it lives here rather than in the
// component, where it can be asserted without a browser or a server.
//
// The strings built here are the query string `listRows` and `countRows` take,
// which is the same vocabulary an application's REST read takes
// (`sc_api::query_string`). That is deliberate: the admin grid is not a second
// reader with a second idea of what `gte` means.

/** The comparisons the server's filter vocabulary takes, longest-lived first. */
export const OPERATORS = [
  "eq",
  "ne",
  "gt",
  "gte",
  "lt",
  "lte",
  "in",
  "nin",
  "like",
  "ilike",
  "is_null",
] as const;

/** One sorted column, in the shape TanStack's sorting state uses. */
export type Sort = { id: string; desc: boolean };

/** The types whose filter box means "contains", rather than "equals".
 *
 * Only the two string types. `ilike` against a `date` or a `uuid` column is not
 * a comparison Postgres has, so defaulting to it would turn a typo in a filter
 * box into a 400 for every other column too. */
const PATTERN_TYPES = new Set(["text", "string"]);

/** The symbol prefixes a filter box takes, longest first so `>=` wins over `>`. */
const SYMBOLS: Array<[string, string]> = [
  [">=", "gte"],
  ["<=", "lte"],
  ["!=", "ne"],
  [">", "gt"],
  ["<", "lt"],
  ["=", "eq"],
];

/**
 * One filter box's text as the `op.value` the server takes, or `null` when the
 * box is empty and filters nothing.
 *
 * Three spellings, in the order they are recognised:
 *
 * - **the vocabulary itself** — `gte.2020-01-01`, `is_null.true`, `in.(1,2,3)`.
 *   Anything the server can be asked, an admin can type, and it goes through
 *   unaltered.
 * - **a comparison symbol** — `>= 100`, `!= draft`. What somebody types in a
 *   spreadsheet, mapped onto the same vocabulary.
 * - **bare text** — `rock`. `ilike.%rock%` on a string column, because a filter
 *   box over text means "contains"; `eq.rock` on anything else, because it does
 *   not mean that on a number and `ilike` is not a comparison a date column has.
 */
export function filterSpec(type: string, raw: string): string | null {
  const text = raw.trim();
  if (text === "") return null;
  const [head, rest] = splitOnce(text, ".");
  if (rest !== null && (OPERATORS as readonly string[]).includes(head)) return text;
  for (const [symbol, op] of SYMBOLS) {
    if (text.startsWith(symbol)) {
      return `${op}.${text.slice(symbol.length).trim()}`;
    }
  }
  return PATTERN_TYPES.has(type) ? `ilike.%${text}%` : `eq.${text}`;
}

/**
 * Every non-empty filter box as the query-string map `listRows` takes: one entry
 * per column, keyed by column name.
 *
 * `types` is the column's declared type, which is all `filterSpec` needs to
 * decide what a bare word means. A box on a column that is no longer there is
 * dropped rather than sent — the server would refuse the whole read by name,
 * which is right for a caller that meant it and wrong for a filter left behind
 * by a field that was deleted.
 */
export function filterQuery(
  types: Record<string, string>,
  drafts: Record<string, string>,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [column, raw] of Object.entries(drafts)) {
    const type = types[column];
    if (type === undefined) continue;
    const spec = filterSpec(type, raw);
    if (spec !== null) out[column] = spec;
  }
  return out;
}

/** How many filter boxes are actually filtering — what the toolbar badges. */
export function activeFilterCount(
  types: Record<string, string>,
  drafts: Record<string, string>,
): number {
  return Object.keys(filterQuery(types, drafts)).length;
}

/**
 * The sorted columns as an `order` key list, or `undefined` when nothing is
 * sorted and the server may answer in its own order.
 *
 * Precedence is the list's own order, which is the order the columns were
 * shift-clicked in — the same rule every spreadsheet has.
 *
 * `tiebreaker` is the table's primary key, appended last unless it is already
 * sorted on. The grid reads in pages by offset, and an offset is only a position
 * in an order the database will give again: unsorted, or sorted on a column with
 * repeats, two reads of "rows 100–199" may disagree, so a scroll would show one
 * row twice and another never. Ending on a unique column makes the order total.
 */
export function orderSpec(sorting: Sort[], tiebreaker: string | null = null): string | undefined {
  const keys = sorting.map((s) => `${s.id}.${s.desc ? "desc" : "asc"}`);
  if (tiebreaker !== null && !sorting.some((s) => s.id === tiebreaker)) {
    keys.push(`${tiebreaker}.asc`);
  }
  return keys.length === 0 ? undefined : keys.join(",");
}

/**
 * The sorting after clicking a column header: none → ascending → descending →
 * none, and a plain click drops every other sorted column.
 *
 * Shift-click keeps them, so a second column becomes a tiebreaker rather than
 * replacing the first — which is the only reason `order` is a list.
 */
export function toggleSort(sorting: Sort[], id: string, additive: boolean): Sort[] {
  const current = sorting.find((s) => s.id === id);
  const others = additive ? sorting.filter((s) => s.id !== id) : [];
  if (current === undefined) return [...others, { id, desc: false }];
  if (!current.desc) return [...others, { id, desc: true }];
  return others;
}

// --- paging ----------------------------------------------------------------

/** How many rows one request fetches. */
export const PAGE_SIZE = 100;

/**
 * The most rows the grid will put in its row model.
 *
 * The grid gives the table library one placeholder row per row of the table, so
 * that a cell's position in the selection is its position in the table and a
 * range survives a scroll. That is an object per row, built once, and there is a
 * size past which building it costs more than the screen is worth — so past this
 * the grid shows the first `ROW_CEILING` rows of the current order and says so.
 * Narrowing with the filter row is the way to reach the rest, which is also how
 * anybody actually finds a row in a table this size.
 */
export const ROW_CEILING = 50_000;

/** The offset and limit of one page. */
export type PageBounds = { offset: number; limit: number };

/** Where page `page` starts and how many rows it asks for. */
export function pageBounds(page: number, pageSize: number = PAGE_SIZE): PageBounds {
  return { offset: page * pageSize, limit: pageSize };
}

/** The page a row index falls in. */
export function pageOf(index: number, pageSize: number = PAGE_SIZE): number {
  return Math.floor(index / pageSize);
}

/**
 * Every page touched by the rows `first`..`last` inclusive — what a scroll to a
 * position has to have before it can draw.
 *
 * Clamped at zero rather than trusting the caller: a virtualizer asked for
 * overscan either side of the first row reports a negative start, and a negative
 * page is an offset the server refuses.
 */
export function pagesCovering(
  first: number,
  last: number,
  pageSize: number = PAGE_SIZE,
): number[] {
  const from = pageOf(Math.max(0, Math.min(first, last)), pageSize);
  const to = pageOf(Math.max(0, first, last), pageSize);
  const pages: number[] = [];
  for (let p = from; p <= to; p += 1) pages.push(p);
  return pages;
}

/** The pages of `wanted` that are neither loaded nor already being fetched. */
export function missingPages(wanted: number[], have: Iterable<number>): number[] {
  const held = new Set(have);
  return wanted.filter((p) => !held.has(p));
}

/**
 * The pages to request now, of the `wanted` ones the viewport covers.
 *
 * At rest, every missing one. While the viewport is moving, only those next to a
 * page already held or on its way: that is a scroll *through* the rows, a wheel
 * or a held arrow key, and the page it is about to need should be asked for
 * before it gets there. A missing page with no held neighbour is a jump — the
 * scrollbar thumb dragged across forty thousand rows — and asking for each of
 * the four hundred pages it passes would queue four hundred reads to draw rows
 * nobody stopped on. Those wait for the scroll to settle, which re-asks.
 */
export function pagesToFetch(
  wanted: number[],
  have: Iterable<number>,
  scrolling: boolean,
): number[] {
  const held = new Set(have);
  const missing = missingPages(wanted, held);
  if (!scrolling || held.size === 0) return missing;
  return missing.filter((p) => held.has(p - 1) || held.has(p + 1));
}

/** `text` split at its first `sep`, or `[text, null]` when there is none. */
function splitOnce(text: string, sep: string): [string, string | null] {
  const at = text.indexOf(sep);
  return at === -1 ? [text, null] : [text.slice(0, at), text.slice(at + sep.length)];
}
