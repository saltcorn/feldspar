import { describe, expect, it } from "vitest";

import { CONTEXT_SEPARATOR } from "./i18n";
import {
  asTranslations,
  inheritedEntry,
  isCovered,
  isTranslated,
  liveCoverage,
  missingKeys,
  newRow,
  placeholderProblem,
  placeholders,
  saveBody,
  singleForm,
  siteSummary,
  sourceText,
  type MessageRow,
} from "./translations";

const row = (key: string, translations: Record<string, unknown> = {}): MessageRow => ({
  key,
  source: sourceText(key),
  context: key.includes(CONTEXT_SEPARATOR) ? key.split(CONTEXT_SEPARATOR)[0] : null,
  sites: [{ file: "src/App.tsx", line: 12 }],
  translations,
  inherited: {},
});

describe("the payload", () => {
  it("narrows a malformed answer rather than throwing at the first missing key", () => {
    const empty = asTranslations(undefined);
    expect(empty.locales).toEqual([]);
    expect(empty.messages).toEqual([]);
    expect(empty.default_locale).toBeNull();

    const partial = asTranslations({ scanned: 4, locales: [{ locale: "fr" }] });
    expect(partial.scanned).toBe(4);
    expect(partial.locales).toHaveLength(1);
  });
});

describe("a key and its English", () => {
  it("is the English itself, or everything after the context separator", () => {
    expect(sourceText("Add a task")).toBe("Add a task");
    expect(sourceText(`verb${CONTEXT_SEPARATOR}Order`)).toBe("Order");
    // The context is a note to the translator; a reader never sees it.
    expect(sourceText(`verb${CONTEXT_SEPARATOR}Order`)).not.toContain("verb");
  });
});

describe("a cell", () => {
  it("shows a string and refuses to flatten plural forms into one box", () => {
    expect(singleForm("Ajouter")).toBe("Ajouter");
    expect(singleForm(undefined)).toBe("");
    expect(singleForm(null)).toBe("");
    // A plural entry has no single form: showing one of them in a text box
    // would save it over the rest.
    expect(singleForm({ one: "1 ligne", other: "{count} lignes" })).toBeNull();
  });

  it("counts an empty translation as untranslated", () => {
    expect(isTranslated(row("Add", { fr: "Ajouter" }), "fr")).toBe(true);
    expect(isTranslated(row("Add", { fr: "" }), "fr")).toBe(false);
    expect(isTranslated(row("Add"), "fr")).toBe(false);
  });
});

describe("the placeholder check", () => {
  it("reads the same holes the renderer would fill", () => {
    expect(placeholders("Delete {name}?")).toEqual(["name"]);
    expect(placeholders("{count} of {count} in {table}")).toEqual(["count", "table"]);
    // `{{` is a literal brace, and a run that is not an identifier is text.
    expect(placeholders("Use {{name}} for a literal")).toEqual([]);
    expect(placeholders("Nothing here")).toEqual([]);
  });

  it("refuses a translation that renamed, dropped or invented one", () => {
    expect(placeholderProblem("Delete {name}?", "Supprimer {name} ?")).toBeNull();
    expect(placeholderProblem("Delete {name}?", "Supprimer {nom} ?")).toMatch(/missing \{name\}/);
    expect(placeholderProblem("Delete {name}?", "Supprimer {nom} ?")).toMatch(/unknown \{nom\}/);
    expect(placeholderProblem("Delete {name}?", "Supprimer ?")).toMatch(/missing \{name\}/);

    // Untranslated is not wrong: it is the normal state of most messages.
    expect(placeholderProblem("Delete {name}?", "")).toBeNull();
    expect(placeholderProblem("Delete {name}?", "   ")).toBeNull();

    // The context is not part of the English, so it is not searched for holes.
    expect(
      placeholderProblem(`verb${CONTEXT_SEPARATOR}Delete {name}?`, "Supprimer {name} ?"),
    ).toBeNull();
  });
});

describe("what a save sends", () => {
  const rows = [
    row("Add", { fr: "Ajouter" }),
    row("Delete", { fr: "Supprimer" }),
    row("Loading…"),
  ];

  it("is the catalogue as it stands, with the edits applied", () => {
    expect(saveBody(rows, "fr", { "Loading…": "Chargement…" })).toEqual({
      Add: "Ajouter",
      Delete: "Supprimer",
      "Loading…": "Chargement…",
    });
  });

  it("removes a key an admin blanked rather than storing an empty string", () => {
    // An empty translation and no translation render the same thing — the
    // English — and storing the first would count as translated and make the
    // coverage figure a lie.
    expect(saveBody(rows, "fr", { Add: "" })).toEqual({ Delete: "Supprimer" });
    expect(saveBody(rows, "fr", { Add: "   " })).toEqual({ Delete: "Supprimer" });
  });

  it("carries a plural entry through untouched", () => {
    const plural = [row("{count} rows", { fr: { one: "{count} ligne", other: "{count} lignes" } })];
    expect(saveBody(plural, "fr", {})).toEqual({
      "{count} rows": { one: "{count} ligne", other: "{count} lignes" },
    });
  });

  it("never mentions an orphan, because the grid never showed one", () => {
    // The server puts orphans back. Nothing here can delete one, which is the
    // point: a key the source stopped using is invisible to the extractor, and
    // a save that wrote only what it saw would throw away a human's work.
    const body = saveBody(rows, "fr", {});
    expect(Object.keys(body)).not.toContain("Gone");
  });
});

describe("what is missing", () => {
  const rows = [row("Add", { fr: "Ajouter" }), row("Delete"), row("Loading…", { fr: "" })];

  it("is every key with no translation", () => {
    expect(missingKeys(rows, "fr")).toEqual(["Delete", "Loading…"]);
    expect(missingKeys(rows, "de")).toEqual(["Add", "Delete", "Loading…"]);
  });

  it("moves as cells are typed, so the figure is not a round trip behind", () => {
    expect(liveCoverage(rows, "fr", {})).toEqual({ translated: 1, total: 3, percent: 33 });
    expect(liveCoverage(rows, "fr", { Delete: "Supprimer" })).toEqual({
      translated: 2,
      total: 3,
      percent: 66,
    });
    // Blanking one takes it back.
    expect(liveCoverage(rows, "fr", { Add: "" }).translated).toBe(0);
    // Nothing to translate is translated.
    expect(liveCoverage([], "fr", {}).percent).toBe(100);
  });
});

describe("where a message is said", () => {
  it("names the first site and counts the rest", () => {
    expect(siteSummary([])).toBe("");
    expect(siteSummary([{ file: "src/App.tsx", line: 12 }])).toBe("src/App.tsx:12");
    expect(
      siteSummary([
        { file: "src/App.tsx", line: 12 },
        { file: "src/pages/Tasks.tsx", line: 4 },
      ]),
    ).toBe("src/App.tsx:12 +1");
  });
});

describe("the shared catalogue under an application's (TODO F6)", () => {
  const withShared = (
    key: string,
    translations: Record<string, unknown>,
    inherited: Record<string, unknown>,
  ): MessageRow => ({ ...row(key, translations), inherited });

  it("shows the shared translation only where the app has none of its own", () => {
    expect(inheritedEntry(withShared("Copy", {}, { de: "Kopieren" }), "de")).toBe("Kopieren");
    expect(inheritedEntry(withShared("Copy", { de: "Duplizieren" }, { de: "Kopieren" }), "de")).toBeUndefined();
    // A blank own value is missing, so the shared one shows through.
    expect(inheritedEntry(withShared("Copy", { de: "" }, { de: "Kopieren" }), "de")).toBe("Kopieren");
    expect(inheritedEntry(withShared("Copy", {}, {}), "de")).toBeUndefined();
  });

  it("counts an inherited key as covered, so Translate missing skips it", () => {
    const rows = [
      withShared("Copy", {}, { de: "Kopieren" }),
      withShared("Add", {}, {}),
      withShared("Save", { de: "Speichern" }, {}),
    ];
    expect(isCovered(rows[0], "de")).toBe(true);
    expect(missingKeys(rows, "de")).toEqual(["Add"]);
    expect(liveCoverage(rows, "de", {}).translated).toBe(2);
    // Clearing an override falls back to the shared value; clearing an
    // un-shared one does not.
    const overridden = [withShared("Copy", { de: "Duplizieren" }, { de: "Kopieren" })];
    expect(liveCoverage(overridden, "de", { Copy: "" }).translated).toBe(1);
    expect(liveCoverage([rows[2]], "de", { Save: "" }).translated).toBe(0);
  });

  it("never saves an inherited value as the app's own", () => {
    const rows = [withShared("Copy", {}, { de: "Kopieren" })];
    expect(saveBody(rows, "de", {})).toEqual({});
  });

  it("adds a shared string once, and not an empty one", () => {
    const rows = [withShared("Copy", {}, {})];
    expect(newRow(rows, "  Sign in ")?.key).toBe("Sign in");
    expect(newRow(rows, "Copy")).toBeNull();
    expect(newRow(rows, "   ")).toBeNull();
  });

  it("narrows a payload from before inherited existed", () => {
    const payload = asTranslations({ messages: [{ key: "A", source: "A", context: null, sites: [], translations: {} }] });
    expect(payload.messages[0].inherited).toEqual({});
  });
});
