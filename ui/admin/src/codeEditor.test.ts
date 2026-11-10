/**
 * What the code editor does with the language a `code` setting declared.
 *
 * Monaco itself is not testable without a browser — it measures fonts and
 * attaches to a DOM node — but the decision *in front of* it is, and it is the
 * one that goes wrong quietly. Three things ride on it:
 *
 * - a `run_python_code` body must open in an editor that knows Python, not in a
 *   plain box with a JavaScript tokenizer over it;
 * - indentation in Python is **syntax**, so an editor that inserted two spaces
 *   where the author writes four (or a tab) produces a body that does not parse,
 *   and the failure appears at fire time rather than while typing;
 * - the TypeScript worker is a 6 MB script and the declarations it answers from
 *   are JavaScript's, so starting it for a Python body would be a download and a
 *   catalog fetch that can never answer anything.
 */

import { describe, expect, it } from "vitest";

import { editorSettings } from "./CodeEditor";

describe("the editor a code setting opens in", () => {
  it("gives JavaScript its grammar, its indentation and its language service", () => {
    expect(editorSettings("javascript")).toEqual({ id: "javascript", tabSize: 2, typed: true });
  });

  it("gives Python its own grammar and four-space indentation, and starts no worker", () => {
    expect(editorSettings("python")).toEqual({ id: "python", tabSize: 4, typed: false });
  });

  it("gives JSON its own grammar, for the registry editor, and no TypeScript worker", () => {
    // Its schema checks come from the JSON worker, which `typed` does not
    // govern: `typed` is whether the catalog is read for completions.
    expect(editorSettings("json")).toEqual({ id: "json", tabSize: 2, typed: false });
  });

  it("falls back to plain text for a language it has no grammar for", () => {
    // A server that declares a third language is ahead of this SPA. An
    // unregistered id colours nothing; plaintext at least does not claim to.
    expect(editorSettings("rust")).toEqual({ id: "plaintext", tabSize: 2, typed: false });
    expect(editorSettings("")).toEqual({ id: "plaintext", tabSize: 2, typed: false });
  });

  it("offers completions in exactly one language, which is where the declarations are", () => {
    // `codeTypes.ts` emits TypeScript; the Python counterpart is a generated
    // `saltcorn.pyi`, carried past this milestone.
    const typed = ["javascript", "python", "rust"].filter((l) => editorSettings(l).typed);
    expect(typed).toEqual(["javascript"]);
  });
});
