import { describe, expect, it } from "vitest";

import { firstAvailable, kindLabel, kindOptions, type KindItem } from "./kinds";

const t = (text: string, args: Record<string, string | number> = {}) =>
  text.replace(/\{(\w+)\}/g, (_, k: string) => String(args[k] ?? ""));

const kinds: KindItem[] = [
  { kind: "data_explorer", label: "Data explorer", available: true, arrives_in: null },
  { kind: "map", label: "Map", available: false, arrives_in: "A5" },
];

describe("the workspace kinds", () => {
  it("lists every kind, the ones not here yet disabled with their milestone", () => {
    expect(kindOptions(kinds, t)).toEqual([
      { value: "data_explorer", label: "Data explorer", disabled: false },
      { value: "map", label: "Map (arrives in A5)", disabled: true },
    ]);
    expect(firstAvailable(kinds)).toBe("data_explorer");
    expect(kindLabel(kinds, "map")).toBe("Map");
    expect(kindLabel(kinds, "mystery")).toBe("mystery");
  });

  it("starts on no kind when none is here yet", () => {
    // Before A2 every kind is still to come: the picker has nothing to start on.
    const none = kinds.map((k) => ({ ...k, available: false, arrives_in: k.arrives_in ?? "A2" }));
    expect(firstAvailable(none)).toBe("");
    expect(kindOptions(none, t).every((o) => o.disabled)).toBe(true);
  });
});
