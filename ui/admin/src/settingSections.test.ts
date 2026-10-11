import { describe, expect, it } from "vitest";

import type { FieldSpec } from "./settings";
import { withSections } from "./settingSections";

const field = (name: string, section?: string): FieldSpec => ({
  name,
  label: name,
  type: "text",
  required: false,
  options: [],
  multiline: false,
  section,
});

const drawn = (spec: FieldSpec[], hidden: string[] = []) =>
  withSections(spec, (f) => !hidden.includes(f.name)).map((item) =>
    item.kind === "heading" ? `# ${item.label}` : item.field.name,
  );

describe("withSections", () => {
  // React Native's settings: the project's, then the native apps'.
  const spec = [
    field("store"),
    field("project"),
    field("mobile_url", "Native apps"),
    field("app_id"),
    field("app_icon"),
  ];

  it("draws a group's heading above its first setting, and none before the first group", () => {
    expect(drawn(spec)).toEqual([
      "store",
      "project",
      "# Native apps",
      "mobile_url",
      "app_id",
      "app_icon",
    ]);
  });

  it("keeps the heading when the setting that opens the group is hidden", () => {
    expect(drawn(spec, ["mobile_url"])).toEqual(["store", "project", "# Native apps", "app_id", "app_icon"]);
  });

  it("draws no heading for a group with nothing shown", () => {
    expect(drawn(spec, ["mobile_url", "app_id", "app_icon"])).toEqual(["store", "project"]);
  });

  it("starts a new group at each heading, even one with the same label", () => {
    const twice = [field("a", "Group"), field("b"), field("c", "Group"), field("d")];
    expect(drawn(twice)).toEqual(["# Group", "a", "b", "# Group", "c", "d"]);
  });

  it("treats a blank heading as none", () => {
    expect(drawn([field("a"), field("b", "  ")])).toEqual(["a", "b"]);
  });
});
