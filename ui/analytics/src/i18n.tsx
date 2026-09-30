// The Analytics UI's half of internationalisation: the admin SPA's runtime
// (`ui/admin/src/i18n.tsx`), copied whole, over the `analytics` domain's own
// catalogues in `src/locales` (analytics TODO A1.14; `docs/I18N.md`).
//
// A copy rather than an import, for the reason every domain has its own: the
// catalogue a bundle loads is found by `import.meta.glob` relative to the file
// that asks, and the two runtimes are held to one behaviour by
// `crates/sc-i18n/fixtures/format.json`, which `i18n.format.test.ts` runs here
// as it does in the admin UI.
//
// What follows is the admin UI's file from its imports down.

import {
  createContext,
  Fragment,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
// `?url`, not a CSS import: see `syncRtlStylesheet`.
import bootstrapRtlHref from "bootstrap/dist/css/bootstrap.rtl.min.css?url";

/** The `\u0004` that separates a disambiguating context from the source text. */
export const CONTEXT_SEPARATOR = "\u0004";

/** A message's arguments: `{name}` is replaced by `args.name`. */
export type Args = Record<string, string | number>;

/**
 * One catalogue entry: a string, or the CLDR plural categories of one.
 *
 * Which categories a locale has is CLDR's answer and not a choice — French has
 * `one` and `other`, Russian has four — so this is a partial record and the
 * selection falls back rather than asserting.
 */
export type Message = string | Partial<Record<Intl.LDMLPluralRule, string>>;

/** One locale's catalogue for one domain: key = the English source text (D1). */
export type Catalogue = Record<string, Message>;

/**
 * Render `message` with `args` (proposal §2, decision D2).
 *
 * - `{identifier}` is a placeholder, where an identifier is an ASCII letter or
 *   `_` followed by letters, digits and `_`.
 * - `{{` is a literal `{`.
 * - Anything else between braces is a literal run, copied out as written.
 * - A closing brace is never ambiguous and therefore never escaped: `}` is
 *   always `}`, and `}}` is two of them.
 * - **A placeholder with no argument renders as written.** A visible `{name}`
 *   is a bug report; an empty string is a mystery.
 *
 * A message is never HTML. React escapes it, exactly as it escapes any other
 * string.
 */
export function format(message: string, args: Args = {}): string {
  // The fast path, and the one almost every message takes: no braces at all.
  if (!message.includes("{")) return message;
  return parts(message, (name) =>
    Object.prototype.hasOwnProperty.call(args, name) ? String(args[name]) : null,
  ).join("");
}

/**
 * The message, scanned once, with each placeholder handed to `resolve`.
 *
 * The one implementation of §2's rules; [`format`] joins what it returns and
 * [`T`] interleaves React nodes into it. Exported so a test can assert the
 * interleaving without a DOM: what `<T values={…}>` does is this function plus
 * a `<Fragment>` per piece. `resolve` answers `null` for a name it
 * has no value for, and a placeholder with no value **renders as written** —
 * the same rule, in both callers, because it is written once.
 */
export function parts<V>(
  message: string,
  resolve: (name: string) => V | null,
): (string | V)[] {
  const out: (string | V)[] = [];
  let literal = "";
  let i = 0;
  const flush = () => {
    if (literal) out.push(literal);
    literal = "";
  };
  while (i < message.length) {
    const ch = message[i];
    if (ch === "{" && message[i + 1] === "{") {
      literal += "{";
      i += 2;
      continue;
    }
    if (ch === "{") {
      const close = message.indexOf("}", i + 1);
      if (close === -1) {
        // Never closed: the rest of the message is text.
        literal += message.slice(i);
        break;
      }
      const run = message.slice(i + 1, close);
      const value = isIdentifier(run) ? resolve(run) : null;
      if (value === null) {
        literal += `{${run}}`;
      } else if (typeof value === "string") {
        literal += value;
      } else {
        flush();
        out.push(value);
      }
      i = close + 1;
      continue;
    }
    literal += ch;
    i += 1;
  }
  flush();
  return out;
}

/** Whether `run` is an identifier — a letter or `_`, then letters, digits, `_`. */
function isIdentifier(run: string): boolean {
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(run);
}

/**
 * The string a catalogue entry renders to, selecting a plural form on `count`.
 *
 * A plain string where plural forms were expected is used as written — a
 * translator who wrote one form meant one form. A category the entry does not
 * have falls back to `other`, and then to whatever the entry does have, because
 * a message in the wrong plural form still says something and a blank says
 * nothing.
 */
export function selectMessage(
  message: Message,
  locale: string,
  args: Args,
): string {
  if (typeof message === "string") return message;
  const count = args.count;
  let category: Intl.LDMLPluralRule = "other";
  if (typeof count === "number") {
    try {
      category = new Intl.PluralRules(locale).select(count);
    } catch {
      category = "other";
    }
  }
  const chosen =
    message[category] ?? message.other ?? Object.values(message)[0] ?? "";
  return chosen;
}

/** The key a `tc("verb", "Order")` is filed under. */
export function contextKey(context: string, text: string): string {
  return `${context}${CONTEXT_SEPARATOR}${text}`;
}

/**
 * The languages written right to left, by their BCP-47 language subtag.
 *
 * A list rather than `Intl.Locale.prototype.getTextInfo`, which is recent
 * enough that a browser this bundle otherwise supports may not have it — and
 * this answer decides `<html dir>`, which is not a thing to get wrong on a
 * browser that is merely a year old. The set is the one with living writing
 * systems; adding to it is adding a string.
 */
const RTL_LANGUAGES = new Set([
  "ar",
  "arc",
  "ckb",
  "dv",
  "fa",
  "ha",
  "he",
  "khw",
  "ks",
  "ps",
  "sd",
  "syr",
  "ug",
  "ur",
  "yi",
]);

/** Which way `tag` is written. */
export function direction(tag: string): "ltr" | "rtl" {
  const language = tag.split("-")[0]?.toLowerCase() ?? "";
  return RTL_LANGUAGES.has(language) ? "rtl" : "ltr";
}

/** The source language, which is also every key in every catalogue (D1). */
export const SOURCE_LOCALE = "en";

/**
 * Every catalogue in `src/locales`, as a lazy `import()` each.
 *
 * `import.meta.glob` rather than a hand-written switch, so shipping a locale is
 * adding a file (the `core` catalogues work the same way, through `build.rs`).
 * The bundler emits one chunk per locale and fetches none of them until a
 * request is served in one.
 */
const CATALOGUES = import.meta.glob<{ default: Catalogue }>("./locales/*.json");

/**
 * Load one locale's catalogue, or `{}` if there is none.
 *
 * A missing catalogue is **not an error**: the key is the English source text,
 * so a locale with no file renders correct English, and a facility whose normal
 * state is "60% translated" must treat that as the design rather than the
 * failure.
 */
export async function loadCatalogue(locale: string): Promise<Catalogue> {
  if (locale === SOURCE_LOCALE) return {};
  const load = CATALOGUES[`./locales/${locale}.json`];
  if (!load) return {};
  try {
    return (await load()).default ?? {};
  } catch {
    return {};
  }
}

/** What `useT` hands a component. */
export type Translator = {
  /** The locale this page is being served in. */
  locale: string;
  /** `ltr` or `rtl`, the same answer `<html dir>` was given. */
  dir: "ltr" | "rtl";
  /** Translate a message. The English *is* the key (D1). */
  t: (text: string, args?: Args) => string;
  /** Translate a message that needs disambiguating from another with the same English. */
  tc: (context: string, text: string, args?: Args) => string;
};

/**
 * The untranslated translator: English, left to right, no catalogue.
 *
 * The default context value, so a component rendered outside a provider — in a
 * unit test, say — still renders its English rather than throwing. There is
 * nothing a catalogue could add to a message whose key is already the answer.
 */
const SOURCE_TRANSLATOR: Translator = {
  locale: SOURCE_LOCALE,
  dir: "ltr",
  t: (text, args) => format(text, args),
  tc: (_context, text, args) => format(text, args),
};

const I18nContext = createContext<Translator>(SOURCE_TRANSLATOR);

/** The `id` of the `<link>` that carries Bootstrap's right-to-left stylesheet. */
export const RTL_STYLESHEET_ID = "sc-bootstrap-rtl";

/**
 * Link Bootstrap's right-to-left stylesheet while the page is `rtl`, and
 * unlink it when it is not.
 *
 * A `<link>` rather than a lazy CSS `import()`: the build has `cssCodeSplit:
 * false`, which folds a lazily imported stylesheet into the one global CSS
 * file, so the "lazy" import put `float: right` on every checkbox of every
 * left-to-right page. A `?url` import is emitted as its own asset and costs a
 * left-to-right page nothing but the string.
 */
export function syncRtlStylesheet(
  doc: Document,
  dir: "ltr" | "rtl",
  href: string = bootstrapRtlHref,
): void {
  const existing = doc.getElementById(RTL_STYLESHEET_ID);
  if (dir !== "rtl") {
    existing?.remove();
    return;
  }
  if (existing) return;
  const link = doc.createElement("link");
  link.id = RTL_STYLESHEET_ID;
  link.rel = "stylesheet";
  link.href = href;
  doc.head.appendChild(link);
}

/**
 * Make `locale`'s catalogue available to everything below, and tell the
 * document what language it is in.
 *
 * `<html lang>` and `<html dir>` are set here because they are one fact about
 * the page and this is the one component that knows it: `lang` is what a screen
 * reader picks a voice from and what a browser offers to translate from, and
 * `dir` is what makes a right-to-left page a right-to-left page rather than a
 * left-to-right page full of Arabic.
 *
 * Children render **immediately**, in English, while the catalogue is in
 * flight. The alternative — a spinner over the whole admin UI until a JSON file
 * lands — would make a translation a thing that costs a paint, and English
 * appearing for 40ms is not worse than nothing appearing for 40ms.
 */
export function I18nProvider({
  locale,
  children,
}: {
  locale: string;
  children: ReactNode;
}): JSX.Element {
  const [catalogue, setCatalogue] = useState<Catalogue>({});
  const dir = direction(locale);

  useEffect(() => {
    let live = true;
    setCatalogue({});
    void loadCatalogue(locale).then((loaded) => {
      if (live) setCatalogue(loaded);
    });
    return () => {
      live = false;
    };
  }, [locale]);

  useEffect(() => {
    document.documentElement.lang = locale;
    document.documentElement.dir = dir;
  }, [locale, dir]);

  // Bootstrap's right-to-left stylesheet, loaded only for a right-to-left
  // locale (task 3.3). Tabler's CSS is a superset of Bootstrap's and is not
  // built RTL, so this mirrors the Bootstrap layer — floats, margins, the grid
  // — underneath it. A full RTL audit of Tabler's own rules is explicitly out
  // of scope for this milestone; Arabic is in the shipped set precisely so the
  // gaps are visible rather than theoretical.
  useEffect(() => {
    syncRtlStylesheet(document, dir);
  }, [dir]);

  const value = useMemo<Translator>(
    () => ({
      locale,
      dir,
      t: (text, args) => lookup(catalogue, locale, text, args),
      tc: (context, text, args) =>
        lookup(catalogue, locale, contextKey(context, text), args, text),
    }),
    [catalogue, locale, dir],
  );

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

/**
 * One lookup: the catalogue, then the key itself.
 *
 * `fallback` is what `tc` renders when nothing is translated — the source text
 * without its context, because the context is a note to the translator and
 * never something a reader sees.
 */
function lookup(
  catalogue: Catalogue,
  locale: string,
  key: string,
  args: Args = {},
  fallback?: string,
): string {
  const entry = catalogue[key];
  if (entry === undefined) return format(fallback ?? key, args);
  return format(selectMessage(entry, locale, args), args);
}

/** The translator for the current locale. */
export function useT(): Translator {
  return useContext(I18nContext);
}

/**
 * A translated message as an element: `<T text="Add a task" />`.
 *
 * The same thing `t()` does, for the places where a call is awkward — a JSX
 * child beside other elements — and, more usefully, the thing the **lint**
 * looks for: a bare English literal sitting in a JSX text node is reported by
 * `feldspar i18n lint`, and wrapping it in this is one of the two ways to
 * answer that report.
 *
 * # Element values, and why they are not optional
 *
 * `values` maps a placeholder to a **React node**:
 *
 * ```tsx
 * <T
 *   text="Each ticked trigger is exposed as {route} on this app."
 *   values={{ route: <code>POST /actions/…</code> }}
 * />
 * ```
 *
 * Without it, a sentence with a link or a `<code>` in the middle has to be cut
 * into three messages — "Each ticked trigger is exposed as", the element, "on
 * this app." — and a translator handed those three fragments cannot move the
 * element, cannot reorder the clause, and in half the languages cannot produce
 * a grammatical sentence at all. One message with a hole in it is the whole
 * difference between a translatable screen and a translated-looking one.
 *
 * `args` and `values` are looked up in that order, so a name in both is a
 * string.
 */
export function T({
  text,
  context,
  args,
  values,
}: {
  text: string;
  context?: string;
  args?: Args;
  values?: Record<string, ReactNode>;
}): JSX.Element {
  const { t, tc } = useT();
  const rendered = context ? tc(context, text, args) : t(text, args);
  if (!values) return <>{rendered}</>;
  // `t` has already substituted `args` and left every other placeholder as
  // written, so what is left to find here is exactly the element holes.
  const pieces = parts<ReactNode>(rendered, (name) =>
    Object.prototype.hasOwnProperty.call(values, name) ? values[name] : null,
  );
  return (
    <>
      {pieces.map((piece, index) => (
        <Fragment key={index}>{piece}</Fragment>
      ))}
    </>
  );
}
