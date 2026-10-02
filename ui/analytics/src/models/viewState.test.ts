import { describe, expect, it } from "vitest";

import { VIEW_KEYS, chosenFit, editorPatch, readEditorView, toggled } from "./viewState";

describe("the model editor's view state", () => {
  it("reads its own keys and ignores the rest", () => {
    expect(
      readEditorView({
        [VIEW_KEYS.collapsed]: ["coefficients", 3],
        [VIEW_KEYS.plots]: ["qq"],
        [VIEW_KEYS.fit]: "f1",
        compare: { other: "screen" },
      }),
    ).toEqual({ collapsed: ["coefficients"], plots: ["qq"], fit: "f1" });
    expect(readEditorView({})).toEqual({ collapsed: [], plots: [], fit: null });
    expect(readEditorView(null)).toEqual({ collapsed: [], plots: [], fit: null });
  });

  it("patches only the keys that changed, removing an empty one", () => {
    expect(editorPatch({ plots: ["qq"] })).toEqual({ [VIEW_KEYS.plots]: ["qq"] });
    expect(editorPatch({ collapsed: [] })).toEqual({ [VIEW_KEYS.collapsed]: null });
    expect(editorPatch({ fit: null })).toEqual({ [VIEW_KEYS.fit]: null });
    expect(editorPatch({})).toEqual({});
  });

  it("toggles a name in a list", () => {
    expect(toggled(["a"], "b")).toEqual(["a", "b"]);
    expect(toggled(["a", "b"], "a")).toEqual(["b"]);
  });

  it("shows the fit the address names, else a remembered one that still exists", () => {
    const fits = [{ id: "f1" }, { id: "f2" }];
    expect(chosenFit("f9", "f1", fits)).toBe("f9");
    expect(chosenFit(undefined, "f2", fits)).toBe("f2");
    expect(chosenFit(undefined, "gone", fits)).toBeNull();
    expect(chosenFit(undefined, null, fits)).toBeNull();
  });
});
