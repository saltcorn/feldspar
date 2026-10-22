/**
 * What the Build command decides before and after the request.
 *
 * Two decisions, both made in the browser: *which* application this store builds
 * (§13.3's derived `source`, matched client-side so the milestone adds no
 * endpoint), and *where* a failed build's errors belong (§16's diagnostics, as
 * store paths the Problems panel can open). Both are pure functions over data the
 * API already returns, which is why neither needs a workbench to test.
 */

import { describe, expect, it } from "vitest";

import { applicationsBuiltFrom } from "./applications";
import { buildDirectoryOf, parseBuildDiagnostics, storePathOf } from "./buildDiagnostics";
import type { ListApplicationsResponse } from "./client";

/** An application as `listApplications` returns one, with only what matters here. */
function application(
  name: string,
  source: { store: string; path: string } | null,
): ListApplicationsResponse[number] {
  return {
    id: `id-${name}`,
    name,
    description: "",
    subdomain: name,
    framework: { name: "react", config: {} },
    extra_frameworks: [],
    tables: [],
    file_stores: [],
    triggers: [],
    streams: [],
    apis: [],
    static_dirs: [],
    uploads: [],
    csp: null,
    attributes: null,
    min_role: 100,
    max_role: 1,
    access_if: "",
    source,
    builds: true,
    installs: true,
    has_views: false,
    targets: [],
  };
}

describe("which application this store builds", () => {
  const applications = [
    application("todo", { store: "apps", path: "todo/web" }),
    application("blog", { store: "apps", path: "blog/web" }),
    application("assets-only", { store: "media", path: "" }),
    application("no-source", null),
  ];

  it("matches on the derived source store, and orders by name", () => {
    const found = applicationsBuiltFrom(applications, "apps");
    expect(found.map((f) => f.application.name)).toEqual(["blog", "todo"]);
    expect(found.map((f) => f.sourcePath)).toEqual(["blog/web", "todo/web"]);
  });

  it("is empty for a store no application is built from", () => {
    expect(applicationsBuiltFrom(applications, "uploads")).toEqual([]);
    // An application with no code framework has no source at all.
    expect(applicationsBuiltFrom([application("db-only", null)], "apps")).toEqual([]);
  });

  it("reads a project at the store root as the empty source path", () => {
    const [found] = applicationsBuiltFrom(applications, "media");
    expect(found.sourcePath).toBe("");
  });

  it("normalizes a source path the way the store spells one", () => {
    const odd = [application("odd", { store: "apps", path: "./web/" })];
    expect(applicationsBuiltFrom(odd, "apps")[0].sourcePath).toBe("web");
  });
});

describe("a failed build's diagnostics", () => {
  /** What `build_app` fails with: its own preamble, then the tools' own output. */
  const tscFailure = [
    "build command `npm run build` failed in /srv/stores/apps/todo/web with exit status: 2",
    "",
    "> build",
    "> tsc --noEmit && vite build",
    "",
    "src/App.tsx(12,15): error TS2322: Type 'string' is not assignable to type 'number'.",
    "src/api/todos.ts(4,3): error TS2339: Property 'nope' does not exist on type 'Todo'.",
  ].join("\n");

  it("names the file, the line and the column tsc reported", () => {
    const found = parseBuildDiagnostics(tscFailure, "todo/web");
    expect(found).toEqual([
      {
        path: "todo/web/src/App.tsx",
        line: 12,
        column: 15,
        severity: "error",
        message: "TS2322: Type 'string' is not assignable to type 'number'.",
      },
      {
        path: "todo/web/src/api/todos.ts",
        line: 4,
        column: 3,
        severity: "error",
        message: "TS2339: Property 'nope' does not exist on type 'Todo'.",
      },
    ]);
  });

  it("finds where the build ran, so absolute paths can be placed in the store", () => {
    expect(buildDirectoryOf(tscFailure)).toBe("/srv/stores/apps/todo/web");
    expect(buildDirectoryOf("something else entirely")).toBeNull();
  });

  it("maps a path the tool gave absolutely, and drops one outside the build", () => {
    const build = "/srv/stores/apps/todo/web";
    expect(storePathOf(`${build}/src/App.tsx`, "todo/web", build)).toBe("todo/web/src/App.tsx");
    expect(storePathOf("src/App.tsx", "todo/web", build)).toBe("todo/web/src/App.tsx");
    expect(storePathOf("./src/App.tsx", "", null)).toBe("src/App.tsx");
    // A file on the server that is not in this store has nothing to point at.
    expect(storePathOf("/usr/lib/node_modules/typescript/lib/lib.d.ts", "todo/web", build)).toBeNull();
    expect(storePathOf("/srv/stores/apps/todo/web/src/App.tsx", "todo/web", null)).toBeNull();
  });

  it("reads the bundler's boxed report, whose message is above the position", () => {
    const rolldown = [
      "build command `npm run build` failed in /srv/apps/web with exit status: 1",
      "error during build:",
      "Build failed with 1 error:",
      "",
      "\u001b[31m[builtin:vite-transform] \u001b[0mExpected `,` or `}` but found `;`",
      // Colour included, because the build ran without a TTY and the bundler
      // emitted it anyway: the parser has to see through it.
      "   \u001b[38;5;246m╭\u001b[0m\u001b[38;5;246m─\u001b[0m\u001b[38;5;246m[\u001b[0m src/main.tsx:2:17 \u001b[38;5;246m]\u001b[0m",
      "   │",
      " 2 │ export default x;",
      "───╯",
    ].join("\n");
    expect(parseBuildDiagnostics(rolldown, "")).toEqual([
      {
        path: "src/main.tsx",
        line: 2,
        column: 17,
        severity: "error",
        message: "[builtin:vite-transform] Expected `,` or `}` but found `;`",
      },
    ]);
  });

  it("reads esbuild's one-line form, warnings included", () => {
    const esbuild = [
      "src/App.tsx:5:12: ERROR: Expected \";\" but found \"}\"",
      "src/App.tsx:9:1: WARNING: Duplicate key \"a\" in object literal",
    ].join("\n");
    expect(parseBuildDiagnostics(esbuild, "web")).toEqual([
      {
        path: "web/src/App.tsx",
        line: 5,
        column: 12,
        severity: "error",
        message: 'Expected ";" but found "}"',
      },
      {
        path: "web/src/App.tsx",
        line: 9,
        column: 1,
        severity: "warning",
        message: 'Duplicate key "a" in object literal',
      },
    ]);
  });

  it("says nothing about output that names no file, rather than guessing", () => {
    const noDiagnostics = [
      "install command `npm install` failed in /srv/apps/web with exit status: 1",
      "npm error code ENOTFOUND",
      "npm error network request to https://registry.npmjs.org/react failed",
      "    at ClientRequest.emit (node:events:518:28)",
    ].join("\n");
    expect(parseBuildDiagnostics(noDiagnostics, "web")).toEqual([]);
  });

  it("reports each problem once, however often the tools repeat it", () => {
    const repeated = [
      "src/App.tsx(3,1): error TS1005: ';' expected.",
      "src/App.tsx(3,1): error TS1005: ';' expected.",
    ].join("\n");
    expect(parseBuildDiagnostics(repeated, "")).toHaveLength(1);
  });
});
