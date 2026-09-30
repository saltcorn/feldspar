import { describe, expect, it } from "vitest";

import { backToList, openDataset, readEditorState, selectOperation } from "./editorState";

describe("a Dataset editor workspace's state", () => {
  it("is the list until a dataset is opened, and remembers the operation selected", () => {
    expect(readEditorState({})).toEqual({ dataset: null, operation: null });
    const open = openDataset({ other: 1 }, "d1");
    expect(readEditorState(open)).toEqual({ dataset: "d1", operation: null });
    const selected = selectOperation(open, "op2");
    expect(readEditorState(selected)).toEqual({ dataset: "d1", operation: "op2" });
    expect(readEditorState(selectOperation(open, ""))).toEqual({ dataset: "d1", operation: "" });
    // What the kind does not own is kept.
    expect(backToList(selected)).toEqual({ other: 1, dataset: null, operation: null });
    expect(readEditorState({ dataset: 7 })).toEqual({ dataset: null, operation: null });
  });
});
