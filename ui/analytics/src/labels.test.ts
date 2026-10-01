import { describe, expect, it } from "vitest";

import { OP_KINDS, SUMMARY_FUNCTIONS, WINDOW_FUNCTIONS } from "./datasets/ops";
import { opKindAbout, opKindName, summaryName, windowFunctionName, workspaceKindName } from "./labels";

const t = (text: string) => text;

describe("the names of things kept as data", () => {
  it("has a translatable name for every kind, function and summary the data lists", () => {
    // The data's English and the literal the extractor sees must agree, or a
    // label added to one list and not the other renders as its key.
    for (const k of OP_KINDS) {
      expect(opKindName(k.kind, t)).toBe(k.label);
      expect(opKindAbout(k.kind, t)).toBe(k.about);
    }
    for (const f of WINDOW_FUNCTIONS) expect(windowFunctionName(f.value, t)).toBe(f.label);
    for (const f of SUMMARY_FUNCTIONS) expect(summaryName(f.value, t)).toBe(f.label);
    expect(workspaceKindName("data_explorer", t)).toBe("Data explorer");
    expect(workspaceKindName("simulation", t)).toBe("Simulation");
    expect(workspaceKindName("mystery", t)).toBe("mystery");
  });
});
