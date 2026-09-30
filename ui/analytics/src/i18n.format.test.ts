// The message format, checked against the corpus the Rust half is checked
// against (proposal §2, task 3.4).
//
// `crates/sc-i18n/fixtures/format.json` is the single source of truth for what
// `{name}`, `{{` and a missing argument mean, and it is run twice: by
// `crates/sc-i18n/tests/format_fixture.rs` over `sc_i18n::format`, and here
// over `format` in `i18n.tsx`. Two implementations of one thing disagree by the
// third bug fixed in one of them; a shared fixture is what makes this instance
// affordable. **Add a case to the fixture before fixing a bug in either half.**
//
// The fixture is read from the repository rather than copied into this tree: a
// copy is a thing that drifts, and the point of the exercise is that it cannot.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import {
  CONTEXT_SEPARATOR,
  contextKey,
  direction,
  format,
  loadCatalogue,
  parts,
  selectMessage,
  type Args,
} from "./i18n";

type Case = {
  name: string;
  message: string;
  args: Args;
  expected: string;
};

const fixture = JSON.parse(
  readFileSync(
    fileURLToPath(
      new URL("../../../crates/sc-i18n/fixtures/format.json", import.meta.url),
    ),
    "utf8",
  ),
) as { cases: Case[] };

describe("format, against the shared corpus", () => {
  it("has cases to run", () => {
    // A fixture that silently became empty would make every assertion below
    // vacuous, which is the one failure a table-driven test cannot report.
    expect(fixture.cases.length).toBeGreaterThan(20);
  });

  for (const testCase of fixture.cases) {
    it(testCase.name, () => {
      expect(format(testCase.message, testCase.args)).toBe(testCase.expected);
    });
  }
});

describe("plural selection", () => {
  it("picks the CLDR category for the count", () => {
    const rows = { one: "{count} row", other: "{count} rows" };
    expect(format(selectMessage(rows, "en", { count: 1 }), { count: 1 })).toBe(
      "1 row",
    );
    expect(format(selectMessage(rows, "en", { count: 4 }), { count: 4 })).toBe(
      "4 rows",
    );
    // French puts 0 in `one`, which is CLDR's answer and not a choice.
    const lignes = { one: "{count} ligne", other: "{count} lignes" };
    expect(selectMessage(lignes, "fr", { count: 0 })).toBe("{count} ligne");
    // …where English puts it in `other`.
    expect(selectMessage(lignes, "en", { count: 0 })).toBe("{count} lignes");
  });

  it("uses a plain string as written where plurals were expected", () => {
    expect(selectMessage("{count} rows", "en", { count: 1 })).toBe("{count} rows");
  });

  it("falls back to `other`, and then to whatever there is", () => {
    // A message in the wrong plural form still says something; a blank says
    // nothing.
    expect(selectMessage({ other: "many" }, "en", { count: 1 })).toBe("many");
    expect(selectMessage({ few: "quelques" }, "en", { count: 1 })).toBe("quelques");
  });

  it("selects `other` when there is no count to select on", () => {
    expect(selectMessage({ one: "a", other: "b" }, "en", {})).toBe("b");
  });
});

describe("the pieces a message with element holes is built from", () => {
  // What `<T values={{…}}>` renders, without a DOM: the sentence is cut at the
  // placeholders it has values for, and everything else stays literal text.
  const hole = { tab: { node: "App settings" } };
  const resolve = (name: string) =>
    Object.prototype.hasOwnProperty.call(hole, name)
      ? hole[name as keyof typeof hole]
      : null;

  it("cuts the sentence at the hole and keeps the text around it", () => {
    expect(parts("Settings are on the {tab} tab.", resolve)).toEqual([
      "Settings are on the ",
      { node: "App settings" },
      " tab.",
    ]);
  });

  it("puts a hole at either end without an empty string beside it", () => {
    expect(parts("{tab}", resolve)).toEqual([{ node: "App settings" }]);
    expect(parts("{tab} tab", resolve)).toEqual([
      { node: "App settings" },
      " tab",
    ]);
  });

  it("leaves a placeholder with no value as written", () => {
    // The same rule `format` follows, because it is the same function: a
    // visible {name} is a bug report and an empty string is a mystery.
    expect(parts("On the {other} tab.", resolve)).toEqual(["On the {other} tab."]);
  });

  it("keeps a literal brace a literal brace", () => {
    expect(parts("{{tab} and {tab}", resolve)).toEqual([
      "{tab} and ",
      { node: "App settings" },
    ]);
  });
});

describe("direction", () => {
  it("is rtl for the languages written right to left", () => {
    expect(direction("ar")).toBe("rtl");
    expect(direction("ar-EG")).toBe("rtl");
    expect(direction("he")).toBe("rtl");
    // The region never decides it: `fa-IR` is Persian wherever it is read.
    expect(direction("fa-IR")).toBe("rtl");
  });

  it("is ltr for everything else, including a tag nobody has heard of", () => {
    expect(direction("en")).toBe("ltr");
    expect(direction("zh-Hans")).toBe("ltr");
    expect(direction("qqq")).toBe("ltr");
    expect(direction("")).toBe("ltr");
  });
});

describe("contextKey", () => {
  it("files a disambiguated message under its context and a separator", () => {
    expect(contextKey("verb", "Order")).toBe(`verb${CONTEXT_SEPARATOR}Order`);
    expect(CONTEXT_SEPARATOR).toBe("\u0004");
  });
});

describe("loadCatalogue", () => {
  it("is empty for the source language, and asks for no file", async () => {
    // D11: the key is the English, so an English admin UI fetches nothing.
    expect(await loadCatalogue("en")).toEqual({});
  });

  it("is empty for a locale with no catalogue, rather than a failure", async () => {
    expect(await loadCatalogue("qqq")).toEqual({});
  });
});
