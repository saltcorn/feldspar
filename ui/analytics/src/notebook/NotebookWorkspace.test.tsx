// The Notebook as it is first drawn: a reopened notebook shows every output
// without running anything, each marked "not run in this session" (there is
// none yet), its run's count beside it, an error with its line, a panel with a
// drag handle, and a non-persisted dataset with Save as dataset and Open in
// explorer. A notebook shared read-only shows the same with nothing to run.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { NotebookWorkspace } from "./NotebookWorkspace";

const inline = {
  kind: "inline",
  definition: { id: "6d1f6a52-0000-4000-8000-000000000001", name: "pricey", base: { kind: "table", table: "houses" } },
};

const state = {
  language: "javascript",
  cells: [
    { id: "t", kind: "text", source: "# Houses\n\nA look at prices." },
    {
      id: "a",
      kind: "code",
      source: 'const d = nb.dataset("houses")\nnb.table(d)',
      outputs: [
        { kind: "dataset", data: inline, columns: ["price"], rows: [[120000], [150000]], total: 2 },
        { kind: "panel", panel: { id: "p", kind: "plot", content: { spec: { data: inline, layers: [] } } } },
      ],
      run: { count: 3, at: "2026-11-20T10:00:00Z", ms: 40 },
    },
    {
      id: "b",
      kind: "code",
      source: "nb.plot({ data: d, x: 'nope' })",
      outputs: [{ kind: "error", message: "there is no column `nope`", line: 1 }],
      run: { count: 4, at: "2026-11-20T10:00:01Z" },
    },
  ],
};

const draw = (readOnly = false) =>
  renderToStaticMarkup(<NotebookWorkspace id="w1" state={state} setState={() => undefined} readOnly={readOnly} />);

describe("the Notebook workspace", () => {
  const html = draw();

  it("renders the text cells' Markdown and the code cells' counts", () => {
    expect(html).toContain("<h1>Houses</h1>");
    expect(html).toContain("[3]");
    expect(html).toContain("[4]");
  });

  it("shows the stored outputs, marked as not run in this session", () => {
    expect(html.match(/data-stale/g)?.length).toBe(2);
    expect(html).toContain('data-output="dataset"');
    expect(html).toContain("120000");
    expect(html).toContain("2 of 2 rows");
    expect(html).toContain('data-panel-kind="plot"');
  });

  it("gives the error its line", () => {
    expect(html).toContain("Line 1:");
    expect(html).toContain("there is no column `nope`");
  });

  it("offers the run controls, dragging, and a dataset's save and explore", () => {
    for (const label of ["Run all", "Interrupt", "Restart", "Save as dataset", "Open in explorer"]) {
      expect(html).toContain(label);
    }
    expect(html).toContain('class="an-drag-handle"');
    expect(html).toContain("No session: nothing is defined yet");
  });

  it("offers nothing that would change a read-only notebook", () => {
    const shared = draw(true);
    for (const label of ["Run all", "Restart", "Save as dataset", "Delete cell"]) {
      expect(shared).not.toContain(label);
    }
    expect(shared).toContain('data-panel-kind="plot"');
  });
});

describe("a dataset output's values", () => {
  it("shows a fraction to six significant digits, and the rest as they are", async () => {
    const { cellText } = await import("./NotebookWorkspace");
    expect(cellText(318923.07692307694)).toBe("318923");
    expect(cellText(0.123456789)).toBe("0.123457");
    expect(cellText(39)).toBe("39");
    expect(cellText(null)).toBe("");
    expect(cellText("a")).toBe("a");
  });
});
