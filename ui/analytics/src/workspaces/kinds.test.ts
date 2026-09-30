import { describe, expect, it } from "vitest";

import { firstAvailable, kindLabel, kindOptions, type KindItem } from "./kinds";

const t = (text: string, args: Record<string, string | number> = {}) =>
  text.replace(/\{(\w+)\}/g, (_, k: string) => String(args[k] ?? ""));

const kinds: KindItem[] = [
  { kind: "dataset_editor", label: "Dataset editor", available: true, arrives_in: null },
  { kind: "data_explorer", label: "Data explorer", available: false, arrives_in: "A2" },
];

describe("the workspace kinds", () => {
  it("lists every kind, the ones not here yet disabled with their milestone", () => {
    expect(kindOptions(kinds, t)).toEqual([
      { value: "dataset_editor", label: "Dataset editor", disabled: false },
      { value: "data_explorer", label: "Data explorer (arrives in A2)", disabled: true },
    ]);
    expect(firstAvailable(kinds)).toBe("dataset_editor");
    expect(kindLabel(kinds, "data_explorer")).toBe("Data explorer");
    expect(kindLabel(kinds, "mystery")).toBe("mystery");
  });
});
