// A tool call laid out for a person to read (a run's step list): its
// arguments as named values rather than JSON, and its result as text that
// wraps, previewed when long.

/** A value laid out to read: text as text, a short word list as tags, a list
 * of sentences as a bullet list, an object as its own name/value rows, a list
 * of objects as numbered blocks of rows. Past a few levels, or for anything
 * else, JSON. */
export type ArgValue =
  | { kind: "text"; text: string }
  | { kind: "tags"; tags: string[] }
  | { kind: "list"; items: string[] }
  | { kind: "object"; rows: ArgumentRow[] }
  | { kind: "objects"; items: ArgumentRow[][] }
  | { kind: "json"; json: string };

/** One named value. */
export type ArgumentRow = { name: string; value: ArgValue };

/** The longest a list of words may get and still be shown as tags. */
const MAX_TAGS = 30;
const MAX_TAG_CHARS = 60;
/** How deep objects are laid out as rows before they are shown as JSON. */
const MAX_DEPTH = 3;

/** A call's arguments, one row per top-level key, in the order given. A value
 * that is not an object is one row named "value". */
export function argumentRows(args: unknown): ArgumentRow[] {
  let value = args;
  if (typeof args === "string") {
    try {
      value = JSON.parse(args);
    } catch {
      return args.trim() === ""
        ? []
        : [{ name: "value", value: { kind: "text", text: args } }];
    }
  }
  if (value === null || value === undefined) return [];
  if (!isObject(value)) return [{ name: "value", value: laidOut(value, 0) }];
  return rowsOf(value, 0);
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function rowsOf(object: Record<string, unknown>, depth: number): ArgumentRow[] {
  return Object.entries(object).map(([name, v]) => ({
    name,
    value: laidOut(v, depth),
  }));
}

function laidOut(value: unknown, depth: number): ArgValue {
  if (typeof value === "string") return { kind: "text", text: value };
  if (
    typeof value === "number" ||
    typeof value === "boolean" ||
    value === null
  ) {
    return { kind: "text", text: String(value) };
  }
  if (depth < MAX_DEPTH && isObject(value)) {
    return { kind: "object", rows: rowsOf(value, depth + 1) };
  }
  if (Array.isArray(value)) {
    const scalars = value.every(
      (item) => typeof item === "string" || typeof item === "number",
    );
    if (scalars) {
      const items = value.map(String);
      const short = items.every(
        (item) => item.length <= MAX_TAG_CHARS && !item.includes("\n"),
      );
      return short && items.length <= MAX_TAGS
        ? { kind: "tags", tags: items }
        : { kind: "list", items };
    }
    if (depth < MAX_DEPTH && value.every(isObject)) {
      return {
        kind: "objects",
        items: value.map((item) => rowsOf(item, depth + 1)),
      };
    }
  }
  return { kind: "json", json: JSON.stringify(value, null, 2) };
}

/** What names one object of a list, for its folded line: its id (or name,
 * key, path) and its title (or another short text field), whichever it has. */
export function itemSummary(rows: ArgumentRow[]): {
  key: string | null;
  label: string | null;
} {
  const text = (names: string[]) => {
    for (const name of names) {
      const row = rows.find((r) => r.name === name);
      if (row && row.value.kind === "text" && row.value.text.trim() !== "") {
        return row.value.text.trim();
      }
    }
    return null;
  };
  const key = text(["id", "key", "name", "path", "slug"]);
  const label = text([
    "title",
    "label",
    "summary",
    "description",
    "text",
    "brief",
  ]);
  return { key, label: label === key ? null : label };
}

/** A result as text: JSON pretty-printed, the loop's `error: ` prefix dropped. */
export function resultText(result: string | null, isError: boolean): string {
  if (result === null) return "";
  const text = isError ? result.replace(/^error: /, "") : result;
  try {
    const parsed: unknown = JSON.parse(text);
    if (typeof parsed === "object" && parsed !== null)
      return JSON.stringify(parsed, null, 2);
  } catch {
    // Not JSON: shown as it came.
  }
  return text;
}

/** The first `lines` lines of `text`, and how many more there are. */
export function preview(
  text: string,
  lines = 12,
): { shown: string; more: number } {
  const all = text.split("\n");
  if (all.length <= lines + 2) return { shown: text, more: 0 };
  return { shown: all.slice(0, lines).join("\n"), more: all.length - lines };
}
