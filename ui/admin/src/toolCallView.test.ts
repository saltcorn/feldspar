// A tool call laid out to read on a run's page.

import { describe, expect, it } from "vitest";

import { argumentRows, itemSummary, preview, resultText } from "./toolCallView";

describe("a call's arguments", () => {
  it("are named values: text as text, short word lists as tags, sentences as a list", () => {
    expect(
      argumentRows({
        pattern: "todo",
        regex: true,
        files: ["src/App.tsx", "src/app.css"],
        notes: [
          "Use only the generated client in src/feldspar; do not edit anything inside it.",
        ],
      }),
    ).toEqual([
      { name: "pattern", value: { kind: "text", text: "todo" } },
      { name: "regex", value: { kind: "text", text: "true" } },
      {
        name: "files",
        value: { kind: "tags", tags: ["src/App.tsx", "src/app.css"] },
      },
      {
        name: "notes",
        value: {
          kind: "list",
          items: [
            "Use only the generated client in src/feldspar; do not edit anything inside it.",
          ],
        },
      },
    ]);
  });

  it("lay out a list of objects as blocks of rows, and only deep nesting as JSON", () => {
    expect(argumentRows({ features: [{ id: "a", files: ["x.ts"] }] })).toEqual([
      {
        name: "features",
        value: {
          kind: "objects",
          items: [
            [
              { name: "id", value: { kind: "text", text: "a" } },
              { name: "files", value: { kind: "tags", tags: ["x.ts"] } },
            ],
          ],
        },
      },
    ]);
    const deep = { a: { b: { c: { d: { e: 1 } } } } };
    const json = JSON.stringify(argumentRows(deep));
    expect(json).toContain('"kind":"json"');
  });

  it("read a JSON string, and show none for an empty call", () => {
    expect(argumentRows('{"id": "sets-list"}')).toEqual([
      { name: "id", value: { kind: "text", text: "sets-list" } },
    ]);
    expect(argumentRows({})).toEqual([]);
    expect(argumentRows(null)).toEqual([]);
  });
});

describe("a call's result", () => {
  it("drops the error prefix and pretty-prints JSON", () => {
    expect(resultText("error: `dir` must be a directory", true)).toBe(
      "`dir` must be a directory",
    );
    expect(resultText('{"a":1}', false)).toBe('{\n  "a": 1\n}');
    expect(resultText("src/a.ts:1: todo", false)).toBe("src/a.ts:1: todo");
  });

  it("is previewed when long, saying how much is left", () => {
    const long = Array.from({ length: 40 }, (_, i) => `line ${i + 1}`).join(
      "\n",
    );
    const { shown, more } = preview(long);
    expect(shown.split("\n")).toHaveLength(12);
    expect(more).toBe(28);
    // A little over the preview is shown whole rather than cut for two lines.
    expect(preview("a\nb\nc", 2).more).toBe(0);
  });
});

describe("an object's folded line", () => {
  it("names it by its id and title, whichever it has", () => {
    const [features] = argumentRows({
      features: [{ id: "a", title: "The A", kind: "feature" }],
    });
    if (features.value.kind !== "objects") throw new Error("objects expected");
    expect(itemSummary(features.value.items[0])).toEqual({
      key: "a",
      label: "The A",
    });
    expect(
      itemSummary([{ name: "kind", value: { kind: "text", text: "x" } }]),
    ).toEqual({
      key: null,
      label: null,
    });
  });
});
