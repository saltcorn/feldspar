import { describe, expect, it } from "vitest";

import { makePanel, readPanelDrag, setPanelDrag, type Transfer } from "../panels/panel";
import { addPanel, readReport, removeBlock } from "./state";

const text = (s: string) => makePanel({ kind: "text", content: { markdown: s } }, s);

describe("the report as a sink (A4.3)", () => {
  it("adds dropped panels at the end or before a block, and removes them", () => {
    let r = readReport({});
    expect(r.blocks).toEqual([]);
    r = addPanel(r, text("a"));
    r = addPanel(r, text("c"));
    r = addPanel(r, text("b"), r.blocks[1].id);
    expect(r.blocks.map((b) => b.panel.title)).toEqual(["a", "b", "c"]);
    r = addPanel(r, text("d"), "no-such-block");
    expect(r.blocks.map((b) => b.panel.title)).toEqual(["a", "b", "c", "d"]);
    r = removeBlock(r, r.blocks[0].id);
    expect(r.blocks.map((b) => b.panel.title)).toEqual(["b", "c", "d"]);
    // The state the server reads for the usage index.
    expect(r.blocks[0]).toMatchObject({ kind: "panel", panel: { kind: "text" } });
  });

  it("reads a stored state leniently, dropping what is not a block", () => {
    const good = { id: "b1", kind: "panel", panel: text("ok") };
    const r = readReport({ blocks: [good, { id: "b2", kind: "panel", panel: { kind: "pie" } }, 7, null] });
    expect(r.blocks).toEqual([good]);
    expect(readReport({ blocks: "x" }).blocks).toEqual([]);
  });

  it("takes a copy when a panel is dragged from one report into another", () => {
    const store = new Map<string, string>();
    const dt: Transfer = {
      get types() {
        return [...store.keys()];
      },
      setData: (k, v) => void store.set(k, v),
      getData: (k) => store.get(k) ?? "",
    };
    const first = addPanel(readReport({}), text("shared"));
    setPanelDrag(dt, first.blocks[0].panel);
    const copy = readPanelDrag(dt);
    if (!copy) throw new Error("a panel");
    const second = addPanel(readReport({}), copy);
    expect(second.blocks[0].panel.id).not.toBe(first.blocks[0].panel.id);
    expect(second.blocks[0].panel.content).toEqual(first.blocks[0].panel.content);
    expect(first.blocks).toHaveLength(1);
  });
});
