// An application's Translations screen, as logic (design §16.1, task 4.4).
//
// The screen is one table: a row per message the application's source says, a
// column per locale it serves, and a cell you can type in. Everything here is
// the part of that with an answer worth testing — what a row is, what is
// missing, what the placeholders say, and what a save sends — so `Translations.tsx`
// is a rendering of decisions made in this file.
//
// # The two checks that happen before a save
//
// The server refuses a translation whose placeholders or plural categories
// differ from its key's (D9's guarantee applies to an admin typing one, not
// only to an LLM answering). Doing the same check here is not a duplicate of
// that: it is what lets the cell go red as it is typed, instead of the whole
// save failing with one key named in a sentence. The server remains the
// authority; this is the part that is fast.

import { CONTEXT_SEPARATOR, parts } from "./i18n";

/** One locale's standing, as the server reports it. */
export type LocaleCoverage = {
  locale: string;
  direction: string;
  translated: number;
  total: number;
  percent: number;
  /** Keys this catalogue has that the source no longer uses. */
  orphans: string[];
};

/** Where a message is said in the source. */
export type MessageSite = { file: string; line: number };

/** One row of the grid. */
export type MessageRow = {
  /** The catalogue key: the English, or `context\u0004English`. */
  key: string;
  /** The English a person reads — the key without its context. */
  source: string;
  /** The disambiguating hint, when the call site used `tc`. */
  context: string | null;
  sites: MessageSite[];
  /** Locale tag → the catalogue entry: a string, or plural forms. */
  translations: Record<string, unknown>;
  /** Locale tag → the instance's shared translation of this key (TODO F6).
   * Served where the application has no translation of its own, and shown
   * greyed with "override" there ({@link inheritedEntry}). Empty on the shared
   * screen itself. */
  inherited: Record<string, unknown>;
};

/** A call site whose message is not a literal, so it can never be extracted. */
export type ExtractProblem = { file: string; line: number; message: string };

/** A bare English literal nothing wraps — what the lint found. */
export type Unwrapped = { file: string; line: number; text: string; message: string };

/** Everything `getTranslations` answers. */
export type Translations = {
  store: string;
  scanned: number;
  truncated: boolean;
  default_locale: string | null;
  locales: LocaleCoverage[];
  messages: MessageRow[];
  problems: ExtractProblem[];
  unwrapped: Unwrapped[];
};

/** The shape the server answers, narrowed. `unknown` in, `Translations` out:
 * the endpoint's payload is a map keyed by English sentences, which is not a
 * struct any schema could have declared. */
export function asTranslations(value: unknown): Translations {
  const o = (value ?? {}) as Partial<Translations>;
  return {
    store: typeof o.store === "string" ? o.store : "",
    scanned: typeof o.scanned === "number" ? o.scanned : 0,
    truncated: o.truncated === true,
    default_locale: typeof o.default_locale === "string" ? o.default_locale : null,
    locales: Array.isArray(o.locales) ? o.locales : [],
    messages: Array.isArray(o.messages)
      ? o.messages.map((m) => ({ ...m, inherited: m.inherited ?? {} }))
      : [],
    problems: Array.isArray(o.problems) ? o.problems : [],
    unwrapped: Array.isArray(o.unwrapped) ? o.unwrapped : [],
  };
}

/** The single form of a catalogue entry, for a one-line text box. A plural
 * entry has no single form, so it answers `null` and the cell says so rather
 * than silently showing one of the forms and saving it over the rest. */
export function singleForm(entry: unknown): string | null {
  if (entry === undefined || entry === null) return "";
  if (typeof entry === "string") return entry;
  return null;
}

/** Whether `row` is translated into `locale` by its own catalogue. */
export function isTranslated(row: MessageRow, locale: string): boolean {
  const entry = row.translations[locale];
  return entry !== undefined && entry !== null && entry !== "";
}

/** What the shared catalogue serves for `row` in `locale`, when the row has no
 * translation of its own — `undefined` otherwise. */
export function inheritedEntry(row: MessageRow, locale: string): unknown {
  if (isTranslated(row, locale)) return undefined;
  const entry = row.inherited?.[locale];
  return entry === null || entry === "" ? undefined : entry;
}

/** Whether `row` is served translated into `locale`: its own translation, or
 * the shared catalogue's. */
export function isCovered(row: MessageRow, locale: string): boolean {
  return isTranslated(row, locale) || inheritedEntry(row, locale) !== undefined;
}

/** The placeholders a message uses, in the order they first appear.
 *
 * `{{` is a literal brace and anything that is not an identifier is a literal
 * run, so this is the same rule `format` renders by — which is the point: a
 * check that disagreed with the renderer would flag messages that work and
 * pass ones that do not. */
export function placeholders(message: string): string[] {
  const seen: string[] = [];
  // The renderer's own scanner, called for its side effect: `parts` hands it
  // exactly the runs it would have substituted, which is why a literal `{{`
  // and a non-identifier run are not placeholders here either. A second
  // implementation would be free to disagree with the thing that renders.
  parts<string>(message, (name) => {
    if (!seen.includes(name)) seen.push(name);
    return null;
  });
  return seen;
}

/** Why `translation` cannot be saved against `key`, or `null` if it can.
 *
 * The same rule the server applies, said in the browser so a cell can go red
 * as it is typed. An **empty** translation is not an error — it is a message
 * nobody has translated yet, which is the normal state of most of them. */
export function placeholderProblem(key: string, translation: string): string | null {
  if (!translation.trim()) return null;
  const wanted = placeholders(sourceText(key)).slice().sort();
  const got = placeholders(translation).slice().sort();
  const missing = wanted.filter((p) => !got.includes(p));
  const extra = got.filter((p) => !wanted.includes(p));
  if (missing.length === 0 && extra.length === 0) return null;
  const parts: string[] = [];
  if (missing.length) parts.push(`missing {${missing.join("}, {")}}`);
  if (extra.length) parts.push(`unknown {${extra.join("}, {")}}`);
  return parts.join("; ");
}

/** The English behind a key: everything after the context separator. */
export function sourceText(key: string): string {
  const at = key.indexOf(CONTEXT_SEPARATOR);
  return at === -1 ? key : key.slice(at + 1);
}

/** The keys of `rows` that `locale` has no translation for — neither its own
 * nor a shared one, which is what **Translate missing** fills. */
export function missingKeys(rows: MessageRow[], locale: string): string[] {
  return rows.filter((r) => !isCovered(r, locale)).map((r) => r.key);
}

/** A new, untranslated row for the shared screen's "Add a string" box, or
 * `null` when the text is empty or already a row. */
export function newRow(rows: MessageRow[], text: string): MessageRow | null {
  const key = text.trim();
  if (!key || rows.some((r) => r.key === key)) return null;
  return { key, source: sourceText(key), context: null, sites: [], translations: {}, inherited: {} };
}

/**
 * The catalogue to save for `locale`: what was already there, with the admin's
 * edits applied.
 *
 * An edit to the empty string **removes** the key rather than storing `""`: an
 * empty translation and no translation render the same thing (the English), and
 * storing the first would count as translated and make the coverage figure a
 * lie.
 */
export function saveBody(
  rows: MessageRow[],
  locale: string,
  edits: Record<string, string>,
): Record<string, unknown> {
  const messages: Record<string, unknown> = {};
  for (const row of rows) {
    const existing = row.translations[locale];
    const edited = Object.prototype.hasOwnProperty.call(edits, row.key)
      ? edits[row.key]
      : undefined;
    const value = edited !== undefined ? edited : existing;
    if (value === undefined || value === null) continue;
    if (typeof value === "string" && !value.trim()) continue;
    messages[row.key] = value;
  }
  // An orphan is not in `rows` by definition, so it is not in here either —
  // and it is not deleted: the *server* puts the orphans back, because the
  // grid never showed them and a screen cannot be asked to preserve something
  // it was never given.
  return messages;
}

/** The coverage of `locale`, recomputed from the grid — so the figure moves as
 * cells are typed rather than after a round trip. */
export function liveCoverage(
  rows: MessageRow[],
  locale: string,
  edits: Record<string, string>,
): { translated: number; total: number; percent: number } {
  let translated = 0;
  for (const row of rows) {
    const edited = Object.prototype.hasOwnProperty.call(edits, row.key)
      ? edits[row.key]
      : undefined;
    let has: boolean;
    if (edited === undefined) has = isCovered(row, locale);
    else if (edited.trim()) has = true;
    // An emptied cell falls back to the shared translation, if there is one.
    else has = inheritedEntry({ ...row, translations: {} }, locale) !== undefined;
    if (has) translated += 1;
  }
  const total = rows.length;
  return { translated, total, percent: total === 0 ? 100 : Math.floor((translated * 100) / total) };
}

/** `tasks/src/App.tsx:12`, or the first of several with a count. */
export function siteSummary(sites: MessageSite[]): string {
  if (sites.length === 0) return "";
  const first = `${sites[0].file}:${sites[0].line}`;
  return sites.length === 1 ? first : `${first} +${sites.length - 1}`;
}
