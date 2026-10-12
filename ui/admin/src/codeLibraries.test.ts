import { describe, expect, it } from "vitest";

import {
  blankDraft,
  draftOf,
  importLine,
  libraryBody,
  scopeLabel,
  sortLibraries,
  useLabel,
  validLibraryName,
  type CodeLibrary,
} from "./codeLibraries";
import { codeLibrary, libDeclarations } from "./codeTypes";

const lib = (name: string, application: string | null = null): CodeLibrary => ({
  id: `${application ?? "i"}-${name}`,
  name,
  description: "",
  code: "",
  application_id: application ? `${application}-id` : null,
  application,
  used_by: [],
});

describe("code libraries", () => {
  it("names follow the server's rule", () => {
    for (const good of ["auth", "_x", "$y", "date-fns"]) expect(validLibraryName(good)).toBe(true);
    for (const bad of ["", "2fa", "a b", "../x", "x".repeat(65)])
      expect(validLibraryName(bad)).toBe(false);
  });

  it("a save sends the whole definition, instance-wide as null", () => {
    const draft = { ...blankDraft(), name: " auth ", description: " who " };
    expect(libraryBody(draft)).toEqual({
      name: "auth",
      description: "who",
      code: draft.code,
      application_id: null,
    });
    const app = draftOf({ ...lib("auth", "one"), code: "exports.a = 1;" });
    expect(libraryBody(app).application_id).toBe("one-id");
    expect(app.id).toBe("one-auth");
  });

  it("lists instance libraries first, then each application's, by name", () => {
    const sorted = sortLibraries([lib("b", "two"), lib("z"), lib("a", "two"), lib("a")]);
    expect(sorted.map((l) => `${l.application ?? "-"}/${l.name}`)).toEqual([
      "-/a",
      "-/z",
      "two/a",
      "two/b",
    ]);
  });

  it("says where a library is visible and who uses it", () => {
    expect(scopeLabel(lib("auth"))).toBe("Instance-wide");
    expect(scopeLabel(lib("auth", "one"))).toBe("App: one");
    expect(useLabel({ kind: "query", name: "me", application: "one" })).toBe("query me in one");
    expect(useLabel({ kind: "trigger", name: "nightly", application: null })).toBe("trigger nightly");
    expect(importLine("auth")).toBe('const { … } = lib("auth");');
  });

  it("types lib() with the names there are", () => {
    expect(libDeclarations(["greet", "auth", "auth"])).toContain(
      'declare function lib(name: "auth" | "greet"): any;',
    );
    expect(libDeclarations([])).toContain("declare function lib(name: string): any;");
  });

  it("a library's editor has exports and module, and no request or payload", () => {
    const declared = codeLibrary([], { library: true }, [], ["auth"]);
    expect(declared).toContain("declare const exports: Record<string, any>;");
    expect(declared).toContain("declare const module: { exports: any };");
    expect(declared).toContain("declare const user:");
    expect(declared).not.toContain("declare const payload");
    expect(declared).not.toContain("declare const request");
    // A query's editor has `lib` too.
    expect(codeLibrary([], { request: true }, [], ["auth"])).toContain('lib(name: "auth")');
  });
});
