// Code libraries as the admin edits them (TODO F3): named, shared JavaScript
// that code queries and triggers import with `lib(name)`. Instance-wide, or one
// application's own — which wins over the instance's of the same name for that
// application's queries. The screen is Settings → Development → Code library.
//
// This module is the screen's logic, kept apart so it is tested without a DOM:
// the draft a form edits, the body a save sends, and how a scope and a use read.

import type { ListCodeLibrariesResponse } from "./client";

/** One library as listed. */
export type CodeLibrary = ListCodeLibrariesResponse[number];

/** One query or trigger that imports a library. */
export type LibraryUse = NonNullable<CodeLibrary["used_by"]>[number];

/** What the form edits: the definition, and the id once it is saved. */
export type LibraryDraft = {
  id: string | null;
  name: string;
  description: string;
  code: string;
  /** The application's id, or `""` for instance-wide. */
  applicationId: string;
};

/** The starting point for a new library: a skeleton that shows the shape. */
export function blankDraft(): LibraryDraft {
  return {
    id: null,
    name: "",
    description: "",
    code:
      "// Assign what other code may import to `exports`.\n" +
      "exports.requireUser = () => {\n" +
      '  if (!user) throw httpError(401, "sign in first");\n' +
      "  return user;\n" +
      "};\n",
    applicationId: "",
  };
}

/** A listed library as a draft to edit. */
export function draftOf(library: CodeLibrary): LibraryDraft {
  return {
    id: library.id,
    name: library.name,
    description: library.description,
    code: library.code,
    applicationId: library.application_id ?? "",
  };
}

/** The server's naming rule, checked before a round trip: what `lib("…")` is
 * written with — a letter, `_` or `$`, then letters, digits, `_`, `$` or `-`. */
export function validLibraryName(name: string): boolean {
  return name.length <= 64 && /^[A-Za-z_$][A-Za-z0-9_$-]*$/.test(name);
}

/** The body a create or update sends. The whole definition, every time. */
export function libraryBody(draft: LibraryDraft): {
  name: string;
  description: string;
  code: string;
  application_id: string | null;
} {
  return {
    name: draft.name.trim(),
    description: draft.description.trim(),
    code: draft.code,
    application_id: draft.applicationId === "" ? null : draft.applicationId,
  };
}

/** Where a library is visible, in words. */
export function scopeLabel(library: CodeLibrary): string {
  return library.application ? `App: ${library.application}` : "Instance-wide";
}

/** One use, in words: `query greetMe in one`, `trigger nightly`. */
export function useLabel(use: LibraryUse): string {
  return use.kind === "query" && use.application
    ? `query ${use.name} in ${use.application}`
    : `${use.kind} ${use.name}`;
}

/** The list in the order the screen shows it: instance-wide first, then by
 * application, each by name. */
export function sortLibraries(libraries: CodeLibrary[]): CodeLibrary[] {
  return [...libraries].sort((a, b) => {
    const scope = (a.application ?? "").localeCompare(b.application ?? "");
    return scope !== 0 ? scope : a.name.localeCompare(b.name);
  });
}

/** How to import a library, for the editor's hint. */
export function importLine(name: string): string {
  const shown = validLibraryName(name) ? name : "name";
  return `const { … } = lib(${JSON.stringify(shown)});`;
}
