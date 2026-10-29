// A cell's value on three sides of the same wire: shown, typed, and pasted.
//
// Rows arrive as arbitrary JSON keyed by column name and go back the same way,
// with the server coercing each value to its column's type. So the grid only has
// to decide two things, and it has to decide them consistently: what a value
// *looks like* in a cell, and what a piece of text *means* when it is typed into
// one. Doing that in the component would mean doing it three times — the editor,
// the paste and the row form all go the same way — so it is one function each,
// here.
//
// The round trip is the property worth holding onto: `parse(kind, display(v))`
// is `v` for every value a cell can hold. Copying a range and pasting it back
// where it came from must change nothing, and that only stays true if the two
// directions are written beside each other.

/** A row as `listRows` answers with it — arbitrary JSON keyed by column name. */
export type RowRecord = Record<string, unknown>;

/** The types whose cells are a tick rather than text. */
const BOOL_TYPES = new Set(["bool"]);

/** The types a cell aligns to the right, as a spreadsheet does with numbers. */
const NUMERIC_TYPES = new Set(["int", "integer", "float", "decimal"]);

/** Whether a column of this type is edited as a checkbox. */
export function isBoolType(type: string): boolean {
  return BOOL_TYPES.has(type);
}

/** Whether a column of this type is shown right-aligned. */
export function isNumericType(type: string): boolean {
  return NUMERIC_TYPES.has(type);
}

/**
 * A JSON value as the text a cell shows and an editor opens with.
 *
 * Null and undefined are the empty cell — not the words "null" and "undefined",
 * which would be indistinguishable from a text column that contains them. An
 * object or an array is its JSON, because a `json` column's value has no shorter
 * honest rendering and the admin who put it there can read it.
 */
export function display(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

/**
 * Text as the JSON to send for a column of `type`.
 *
 * Empty is null everywhere: clearing a cell means clearing it, and the server
 * refuses the null if the column will not have one, which is the right place for
 * that to be decided.
 *
 * Otherwise the column's own type decides, rather than the shape of the text:
 * `0` in a `bool` column is false and `0` in a `text` column is the string
 * `"0"`. Reading the text instead would make a `text` column full of digits come
 * back as numbers after a paste — which is the round trip this module exists to
 * keep.
 */
export function parseCell(type: string, raw: string): unknown {
  const text = raw.trim();
  if (text === "") return null;
  if (isBoolType(type)) return parseBool(text);
  if (isNumericType(type)) {
    const n = Number(text);
    // Not a number stays text: the server's own coercion gives the better error
    // ("`pages` expects an integer"), naming the column, than anything here can.
    return Number.isFinite(n) ? n : raw;
  }
  if (type === "json") {
    try {
      return JSON.parse(text) as unknown;
    } catch {
      return raw;
    }
  }
  return raw;
}

/** `true`/`false`/`t`/`f`/`yes`/`no`/`1`/`0`, in any case; anything else stays text. */
function parseBool(text: string): unknown {
  const lower = text.toLowerCase();
  if (["true", "t", "yes", "y", "1"].includes(lower)) return true;
  if (["false", "f", "no", "n", "0"].includes(lower)) return false;
  return text;
}

/**
 * Whether two JSON cell values are the same — what decides that an edit was not
 * an edit and needs no write.
 *
 * Structural rather than `===`, because a `json` column's value is an object and
 * two equal objects are two objects.
 */
export function sameValue(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if ((a === null || a === undefined) && (b === null || b === undefined)) return true;
  return JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
}

// --- dates and times -------------------------------------------------------
//
// A `date`, `time` or `timestamp` cell is edited through the browser's own
// picker (`<input type="date">`, `"time"`, `"datetime-local"`), and each picker
// has a value format of its own that is not quite the wire's. A date is the
// same on both sides. A time arrives as `10:30:00` (or with a fraction) and the
// picker wants `HH:MM[:SS]`. A timestamp is the hard one: it arrives as an
// RFC 3339 instant in UTC, and a `datetime-local` picker holds a *wall-clock*
// reading with no zone — so the picker is filled with the instant as read in the
// browser's time zone, and what comes back out is turned into an instant again
// the same way. Showing an admin UTC in a picker that does not say so would put
// every timestamp an hour or nine off what they meant.

/** The picker a temporal column is edited through, or `null` for any other type. */
export function pickerType(type: string): "date" | "time" | "datetime-local" | null {
  switch (type) {
    case "date":
      return "date";
    case "time":
      return "time";
    case "timestamp":
      return "datetime-local";
    default:
      return null;
  }
}

/** Two digits. */
function pad(n: number): string {
  return String(n).padStart(2, "0");
}

/** A `Date` as the local wall-clock reading `datetime-local` takes, to the second. */
function localReading(d: Date): string {
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
    `T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  );
}

/**
 * A stored temporal value as the text its picker opens with — `""` for an empty
 * cell, and for a value the picker could not hold (it then opens empty rather
 * than on a wrong date).
 */
export function toPicker(type: string, value: unknown): string {
  const text = display(value).trim();
  if (text === "") return "";
  switch (pickerType(type)) {
    case "date": {
      const m = /^\d{4}-\d{2}-\d{2}/.exec(text);
      return m ? m[0] : "";
    }
    case "time": {
      const m = /^(\d{2}:\d{2})(:\d{2})?/.exec(text);
      return m ? m[1] + (m[2] ?? ":00") : "";
    }
    case "datetime-local": {
      const d = new Date(text);
      return Number.isNaN(d.getTime()) ? "" : localReading(d);
    }
    default:
      return text;
  }
}

/** A picker's text as the JSON to send: `null` when it was cleared, and a
 * timestamp as the UTC instant its local reading means. */
export function fromPicker(type: string, raw: string): unknown {
  const text = raw.trim();
  if (text === "") return null;
  switch (pickerType(type)) {
    case "time":
      // `HH:MM` when the seconds are zero, which some browsers drop.
      return /^\d{2}:\d{2}$/.test(text) ? `${text}:00` : text;
    case "datetime-local": {
      // No zone in the text, so `Date` reads it as local — which it is.
      const d = new Date(text);
      return Number.isNaN(d.getTime()) ? raw : d.toISOString();
    }
    default:
      return text;
  }
}

/** What a temporal cell shows: a timestamp as its local reading (`2024-05-01
 * 12:00:00`), the same one its picker opens on, and anything else as itself. */
export function displayTemporal(type: string, value: unknown): string {
  if (pickerType(type) !== "datetime-local") return display(value);
  const reading = toPicker(type, value);
  return reading === "" ? display(value) : reading.replace("T", " ");
}
